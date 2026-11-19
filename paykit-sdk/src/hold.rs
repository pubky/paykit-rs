//! The hold after a restore (Paykit spec 7.2, "What a restore loses"), one-time branch, and
//! the same hold after a `missed` item.
//!
//! A backup can predate a payee's cancellation of a request it holds open: the restored app
//! would see the request payable. So every restore from a backup holds every open request,
//! as the restore itself sets it: [`crate::protocol::PaykitProtocol`]'s restore step adds the
//! hold to the writes it installs, in the restore's one commit, so no restore can leave it
//! out. The hold stays in force until the client's verdict on the restore is known
//! (`App::restore_held_cursor`; Switch spec 12.4, "The held cursor"): a clean one, the first
//! feed page read after the restore reporting a cursor at or behind the backup's position,
//! lifts it with no question to the user and no re-send asked for it, since no entry after
//! the backup's position was passed; ahead or unknown leaves it as below
//! ([`Paykit::take_restore_verdict`]). Every write carries the restored cursor move, which
//! would leave the verdict unknown, so while it isn't known a call that writes or reconciles
//! makes the first read itself first ([`Paykit::first_read_before_writing`]). The holds the
//! backup held itself, of an earlier restore or a `missed` item, cover entries before its
//! position and are carried whatever the verdict, with the user's confirmations of them
//! (`hb/`), which never release this restore's own hold. A `missed` item (Switch spec 8.2)
//! shows the service deleted entries before the app read them, a payee's cancellation
//! possibly among them: the protocol's `missed` step sets the same hold (`mh`) and drops
//! every confirmation of an earlier one, in the commit that passes the item. An item whose
//! end isn't known (the client's `u64::MAX`) holds every open request (`mu`) until a read
//! that completes bounds it by the service time verified then
//! ([`Paykit::bound_missed_hold`]). While a request is held:
//!
//! - no automatic attempt is made on it, and a dispatch made before is not repeated at a
//!   resume;
//! - a manual one only once the user has seen that the payee may have cancelled it
//!   ([`crate::PayOptions::possibly_cancelled_confirmed`]), which ends the hold for that
//!   request;
//! - a cancellation the record shows, or the app keeps, stops it for good, and no
//!   confirmation overrides it (Paykit spec 8.1).
//!
//! After a restore whose verdict isn't clean, once the records are read again, the app
//! asks every counterparty with an open request to re-send what its inbox accepted from
//! the acceptance time of the last entry the backup's cursor had passed, less the clock
//! tolerance (Paykit spec 9). A cancellation that comes back is kept and placed as any
//! other. The asks wait for the verdict, as the widening of the block marks does
//! (`marks.rs`): none goes while it isn't known, and none for a clean restore. A host that
//! ends reconcile mode before any verdict is known gets none from the client any more, so
//! the restore counts as not clean then ([`Paykit::end_reconcile`]).

use paykit_lib::collections::Collection;
use paykit_lib::records::ActivityRecord;
use pubky_switch_client::app::Protocol as _;
use pubky_switch_client::app::{
    AppError, Collection as SwitchCollection, HeldCursor, PolicyFamily, RecordContent,
    Refusal as ClientRefusal, Unopened,
};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::records::{AppCert, CLOCK_TOLERANCE_MS};
use pubky_switch_core::scopes::Permission;
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};

use crate::allowances::{Consent, ConsentEntry};
use crate::error::{PaykitError, Refusal, Result};
use crate::marks::WideningRead;
use crate::paykit::Paykit;
use crate::requests::{cancelled, rejected, RequestStatus};
use crate::state::{key, Tx};

/// A request held after a restore or a `missed` item, as the user is shown it
/// ([`Refusal::HeldAfterRestore`], [`crate::RequestView::held`]; Paykit spec 7.2, "What a
/// restore loses"): its record, and the holds in force over it then. The user's answer
/// carries it back ([`crate::PayOptions::with_possibly_cancelled_confirmed`],
/// [`crate::AcceptOptions::with_possibly_cancelled_confirmed`],
/// [`Paykit::confirm_possibly_cancelled`]), and ends only that hold: once a newer restore
/// or `missed` item holds the request again, or a clean verdict a later report ends, the
/// answer is refused with the hold in force now, and nothing is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldRequest {
    record: [u8; 32],
    epoch: HoldEpoch,
    restored: bool,
    missed_bound_ms: Option<u64>,
    missed_end_unknown: bool,
    newest_missed_item_ms: Option<u64>,
}

/// Identifies the restore or missed item that most recently established a hold. Times
/// alone cannot distinguish two gaps at the same service time or two restores of a backup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HoldEpoch(#[serde(with = "serde_b64")] [u8; 16]);

impl HoldEpoch {
    pub(crate) fn new() -> pubky_switch_core::Result<Self> {
        pubky_switch_core::enc::random_bytes().map(Self)
    }
}

impl HeldRequest {
    /// The request's activity record key.
    pub fn record(&self) -> [u8; 32] {
        self.record
    }
}

/// The hold a restore set (`p/rh`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RestoreHold {
    /// From when the counterparties are asked to re-send: the acceptance time of the last
    /// entry the backup's cursor passed, less the clock tolerance.
    pub(crate) resend_from_ms: u64,
    /// The re-sends are all queued.
    pub(crate) asks_queued: bool,
    /// The counterparties asked already while a record of the copy was sealed, in
    /// increasing order ([`Paykit::queue_restore_asks`]).
    pub(crate) asked: Vec<[u8; 32]>,
    /// Whether the backup held a restore's hold in force itself, of an earlier restore
    /// that isn't clean, or one of a backup before it: that hold, which covers entries
    /// before this backup's position, holds every open request as before, whatever this
    /// restore's verdict, and a confirmation the backup holds (`hb/`) releases it.
    pub(crate) earlier: bool,
    /// Whether that earlier restore's re-sends were still to be asked for in the backup:
    /// they are asked for, from the earlier of the two times, whatever this restore's
    /// verdict.
    pub(crate) earlier_asks: bool,
    /// The client's verdict on this restore, as Paykit took it
    /// ([`Paykit::take_restore_verdict`]).
    pub(crate) verdict: RestoreVerdict,
    /// The start of this restore's block window until its marks and floor are applied.
    /// A clean restore keeps it in case a later cursor report ends the clean verdict.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) widening_from_ms: Option<u64>,
    /// The first read of the lists after this restore, even if it preceded the verdict.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) widening_read: Option<WideningRead>,
    /// A widening a previous restore owed when the backup was taken.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) earlier_widening: Option<OwedWidening>,
    /// The earliest start among previous restores still waiting for their first list read.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) earlier_unread_from_ms: Option<u64>,
}

/// A restore's widening of the block marks owed as of a read of the lists
/// ([`RestoreHold::earlier_widening`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwedWidening {
    /// The widening's start.
    pub(crate) from_ms: u64,
    /// The read it is applied as of.
    pub(crate) read: WideningRead,
}

impl OwedWidening {
    /// One widening covering `self` and `other`: from the earlier start, as of the later
    /// read, with the counterparties either read's deny list holds.
    pub(crate) fn covering(self, other: Self) -> Self {
        let mut deny = self.read.deny;
        deny.extend(other.read.deny);
        deny.sort_unstable();
        deny.dedup();
        Self {
            from_ms: self.from_ms.min(other.from_ms),
            read: WideningRead {
                at_ms: self.read.at_ms.max(other.read.at_ms),
                deny,
            },
        }
    }
}

/// The client's verdict on a restore, as Paykit took it (Switch spec 12.4, "The held
/// cursor"; `App::restore_held_cursor`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestoreVerdict {
    /// Not known yet: the hold is in force, as after a restore that isn't clean.
    Open,
    /// Clean: the hold is lifted; it holds nothing and asks for nothing.
    Clean,
    /// Ahead or unknown, or a clean verdict a later report ended: the hold is in force for
    /// good.
    NotClean,
}

impl RestoreHold {
    /// The hold of a restore whose backup's cursor had passed entries accepted up to
    /// `cursor_time_ms`, over `before`, the hold the backup held, if any.
    pub(crate) fn of(cursor_time_ms: u64, before: Option<&RestoreHold>) -> Self {
        let own = resend_from(cursor_time_ms);
        let earlier_asks = before.is_some_and(RestoreHold::asks_owed);
        Self {
            resend_from_ms: match before {
                Some(before) if earlier_asks => own.min(before.resend_from_ms),
                _ => own,
            },
            asks_queued: false,
            asked: Vec::new(),
            earlier: before.is_some_and(RestoreHold::holds_all),
            earlier_asks,
            verdict: RestoreVerdict::Open,
            widening_from_ms: Some(own),
            widening_read: None,
            earlier_widening: before.and_then(RestoreHold::owed_before),
            earlier_unread_from_ms: before.and_then(|hold| {
                let own = hold
                    .widening_from_ms
                    .filter(|_| hold.in_force() && hold.widening_read.is_none());
                own.into_iter().chain(hold.earlier_unread_from_ms).min()
            }),
        }
    }

    /// Whether re-sends are still to be asked for: this restore's while its hold is in
    /// force, or an earlier one's the backup owed. What a backup of this state carries over
    /// ([`RestoreHold::earlier_asks`]).
    pub(crate) fn asks_owed(&self) -> bool {
        !self.asks_queued && (self.in_force() || self.earlier_asks)
    }

    /// Whether the re-sends owed are asked for now: this restore's only once a verdict
    /// that isn't clean is known, so a clean restore asks nothing for itself; an earlier
    /// one's the backup owed whatever this verdict.
    pub(crate) fn asks_due(&self) -> bool {
        !self.asks_queued && self.has_resend_duty()
    }

    fn has_resend_duty(&self) -> bool {
        self.verdict == RestoreVerdict::NotClean || self.earlier_asks
    }

    /// Whether this restore's own hold holds: until a clean verdict lifts it.
    pub(crate) fn in_force(&self) -> bool {
        self.verdict != RestoreVerdict::Clean
    }

    /// Whether the hold holds every open request, this restore's own or an earlier one the
    /// backup held: what a backup of this state carries over ([`RestoreHold::earlier`]).
    pub(crate) fn holds_all(&self) -> bool {
        self.in_force() || self.earlier
    }

    /// The widening still to be applied as of the first read it kept, once a later report
    /// ended its clean verdict: its start and that read.
    pub(crate) fn widening_owed(&self) -> Option<OwedWidening> {
        match (self.verdict, self.widening_from_ms, &self.widening_read) {
            (RestoreVerdict::NotClean, Some(from_ms), Some(read)) => Some(OwedWidening {
                from_ms,
                read: read.clone(),
            }),
            _ => None,
        }
    }

    /// What a restore from a backup holding this hold still owes of its widenings: this
    /// hold's own owed one, and an earlier one it carried, kept as one that covers both.
    fn owed_before(&self) -> Option<OwedWidening> {
        let own = self
            .widening_from_ms
            .zip(self.widening_read.clone())
            .filter(|_| self.in_force())
            .map(|(from_ms, read)| OwedWidening { from_ms, read });
        match (own, self.earlier_widening.clone()) {
            (Some(own), Some(earlier)) => Some(own.covering(earlier)),
            (own, earlier) => own.or(earlier),
        }
    }

    /// Earlier re-sends belong to a carried hold. Partial or completed progress
    /// belongs to a restore that had re-sends to queue.
    pub(crate) fn is_valid(&self) -> bool {
        (!self.earlier_asks || self.earlier)
            && (!self.asks_queued || self.has_resend_duty())
            && (self.asked.is_empty() || self.asks_due())
            && self.asked.windows(2).all(|pair| pair[0] < pair[1])
            && self.in_order()
    }

    /// A stored read must belong to a window and be at or after its start.
    fn in_order(&self) -> bool {
        (self.verdict == RestoreVerdict::NotClean || self.widening_from_ms.is_some())
            && self.widening_read.as_ref().is_none_or(|read| {
                self.widening_from_ms
                    .is_some_and(|from_ms| read.at_ms >= from_ms)
            })
            && self
                .earlier_widening
                .as_ref()
                .is_none_or(|owed| owed.read.at_ms >= owed.from_ms)
    }
}

/// From when a restore whose backup's cursor had passed entries accepted up to
/// `cursor_time_ms` asks its counterparties to re-send: that time less the clock tolerance.
pub(crate) fn resend_from(cursor_time_ms: u64) -> u64 {
    cursor_time_ms.saturating_sub(CLOCK_TOLERANCE_MS)
}

/// The hold a `missed` item set (`p/mh`), bounded. An item whose end isn't known sets `p/mu`
/// instead, which holds every open request until [`Paykit::bound_missed_hold`] writes this.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MissedHold {
    /// No earlier than the acceptance time of any entry the items told of: an item's last
    /// acceptance time (`MissedEntries::accepted_until_ms`), or, for one whose end isn't
    /// known, a service time verified after the read that handed it.
    pub(crate) bound_ms: u64,
}

impl MissedHold {
    /// The hold bounded by `bound_ms` ([`MissedHold::bound_ms`]).
    pub(crate) fn bounded_by(bound_ms: u64) -> Self {
        Self { bound_ms }
    }

    /// Whether the open request `request` is held (Paykit spec 7.2, 9). One the app held at
    /// the item, its inbox having accepted it no later than the bound, is held. One first
    /// seen after the item is held only when its signed `created_at` is earlier than the
    /// bound plus the clock tolerance: a request created later can't have had its
    /// cancellation among the entries missed. A request with no acceptance time is held.
    pub(crate) fn holds(&self, request: &paykit_lib::records::StoredMessage) -> bool {
        let bound = self.bound_ms;
        let held_at_item = request.accepted_at().is_none_or(|at| at <= bound);
        held_at_item || request.message().created_at_ms < bound.saturating_add(CLOCK_TOLERANCE_MS)
    }
}

/// Which side waits for a wider certificate's scope keys ([`Paykit::scope_keys_awaited`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeysAwaited {
    /// The payer's records, claims or `paykit_pay` key: a resume writes nothing to the switch.
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
        let settlements = crate::activity::family(Collection::Settlements);
        let lists = crate::activity::family(Collection::PaymentLists);
        let floor = crate::activity::family(Collection::BlockFloor);
        let allowances = crate::activity::family(Collection::Allowances);
        match self {
            Self::Payer => vec![payer, lists, floor],
            Self::Payee => vec![payee, settlements],
            Self::Allowances => vec![allowances],
            Self::All => vec![payer, payee, lists, floor, allowances, settlements],
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
    /// the app lacks the current key of `paykit_pay`, the certificate reads claims the one listed
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
        // Without the current `paykit_pay` key the payer's records can't be written, as after a
        // data epoch rotation.
        let pay_key = self
            .app
            .scope_key(self.app.data_epoch(), "paykit_pay")
            .is_some();
        // Claims read now and not under the certificate listed under. Today this follows from the
        // payer collections' term or from `paykit_pay` not held, where a resume reads nothing:
        // `claims.read` comes with `paykit_pay`, which reads `payer_activity`, or with
        // `paykit_allowance` (paykit-lib `scopes.rs`), which doesn't hold `paykit_pay`. It is kept
        // so the rule doesn't lean on that table.
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
    /// [`Paykit::scope_keys_awaited`] gave it): the payer's records, claims or `paykit_pay` key
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
    /// After a restore every open request is held until the client's verdict shows the
    /// restore clean, which lifts the hold ([`Paykit::take_restore_verdict`]); after any
    /// other, every open request is held, one first seen after the restore too: no
    /// time the app can see proves a request new, since its creation time is the
    /// counterparty's and a request from before the backup can come again with a later
    /// acceptance time (Paykit spec 7.2, "What a restore loses": nothing releases the hold
    /// automatically). After a `missed` item, a request first seen after it is held only
    /// when created before the item's last acceptance time plus the clock tolerance
    /// ([`MissedHold::holds`]); while the end of an item isn't known (`mu`), every open
    /// request is held, as after a restore.
    ///
    /// A restore carries the holds its backup held: an earlier restore's
    /// ([`RestoreHold::earlier`]) and a `missed` item's (`mh`, `mu`), with the user's
    /// confirmations of them (`hb/`). This restore's own hold, while in force, is released
    /// only by a confirmation given since it (`hc/`); a carried one by either.
    pub(crate) async fn held(
        &self,
        record_key: &[u8; 32],
        record: &ActivityRecord,
    ) -> Result<bool> {
        let hold = self.local_get::<RestoreHold>(key::RESTORE_HOLD).await?;
        let own = hold.as_ref().is_some_and(RestoreHold::in_force);
        let unbounded = hold.as_ref().is_some_and(RestoreHold::holds_all)
            || self
                .local_get::<bool>(key::MISSED_END_UNKNOWN)
                .await?
                .is_some();
        if !unbounded {
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
        if self
            .local_get::<bool>(&key::hold_confirmed(record_key))
            .await?
            .is_some()
        {
            return Ok(false);
        }
        Ok(own
            || self
                .local_get::<bool>(&key::hold_confirmed_before(record_key))
                .await?
                .is_none())
    }

    /// The hold over the request at `record_key`, as [`HeldRequest`] shows it: what
    /// [`Paykit::held`] holds it by now. Every restore and every `missed` item changes it,
    /// and so does a clean verdict a later report ends.
    pub(crate) async fn held_request(&self, record_key: &[u8; 32]) -> Result<HeldRequest> {
        let hold = self.local_get::<RestoreHold>(key::RESTORE_HOLD).await?;
        let newest_missed_item_ms = self
            .local_scan::<crate::protocol::MissedItemMark>(key::MISSED_ITEMS)
            .await?
            .iter()
            .map(|(_, mark)| mark.from_ms)
            .max();
        Ok(HeldRequest {
            record: *record_key,
            epoch: self
                .local_get(key::HOLD_EPOCH)
                .await?
                .ok_or_else(PaykitError::corrupt)?,
            restored: hold.as_ref().is_some_and(RestoreHold::in_force),
            missed_bound_ms: self
                .local_get::<MissedHold>(key::MISSED_HOLD)
                .await?
                .map(|missed| missed.bound_ms),
            missed_end_unknown: self
                .local_get::<bool>(key::MISSED_END_UNKNOWN)
                .await?
                .is_some(),
            newest_missed_item_ms,
        })
    }

    /// Whether the request at `record_key`, its record `record`, may go on over its hold:
    /// not held, or held as the user was shown it and confirmed, `confirmed`. `Ok(None)` when
    /// not held; `Ok(Some(key))` with the record key whose hold the confirmation ends.
    ///
    /// # Errors
    ///
    /// [`Refusal::HeldAfterRestore`] with the hold in force now, for a held request the
    /// user didn't confirm as held so; a store failure.
    pub(crate) async fn pass_hold(
        &self,
        record_key: &[u8; 32],
        record: &ActivityRecord,
        confirmed: Option<&HeldRequest>,
    ) -> Result<Option<[u8; 32]>> {
        if !self.held(record_key, record).await? {
            return Ok(None);
        }
        let held = self.held_request(record_key).await?;
        if confirmed != Some(&held) {
            return Err(Refusal::HeldAfterRestore {
                held: Box::new(held),
            }
            .into());
        }
        Ok(Some(*record_key))
    }

    /// Bound the hold of a `missed` item whose end isn't known (`mu`), at a receive whose
    /// read completed: by the highest service time the app verified (Switch spec 10.1), no
    /// earlier than any acceptance time of the entries the item stands for, since the
    /// client noted the time of the answer that showed the item, or of a later one, before
    /// the commit that passed it. The bound is the later of that time and the bound of an
    /// earlier item, which the `missed` step left in place; written, with `mu` cleared, in
    /// one commit, so a crash before it leaves every open request held. While no service
    /// time is verified, nothing changes: every open request stays held.
    pub(crate) async fn bound_missed_hold(&mut self) -> Result<()> {
        if self
            .local_get::<bool>(key::MISSED_END_UNKNOWN)
            .await?
            .is_none()
        {
            return Ok(());
        }
        let Some(verified) = self.service_time() else {
            return Ok(());
        };
        let earlier = self
            .local_get::<MissedHold>(key::MISSED_HOLD)
            .await?
            .map_or(0, |hold| hold.bound_ms);
        let mut tx = Tx::default();
        tx.put(
            key::MISSED_HOLD,
            &MissedHold::bounded_by(earlier.max(verified)),
        )?;
        tx.delete(key::MISSED_END_UNKNOWN);
        self.local_commit(tx).await
    }

    /// The user has seen that the payee may have cancelled `request` since the backup the
    /// app was restored from, or a `missed` item, and lets its payment go on: the request's
    /// hold ends, as a payment confirmed with
    /// [`crate::PayOptions::possibly_cancelled_confirmed`] ends it, so a dispatch made before
    /// the hold is repeated at a later resume (Paykit spec 7.2). It never overrides a
    /// cancellation the record shows or the app keeps. A later `missed` item holds the
    /// request again. `held` must be the exact value the user saw, from
    /// [`crate::RequestView::held`] or [`Refusal::HeldAfterRestore`].
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] while the app can't tell whether it holds the request: its record
    /// is sealed under a scope key the app doesn't hold yet, or a value that doesn't read, or
    /// it is missing from a copy that may lack records; [`Refusal::NotFound`] when the copy
    /// holds no record of it. [`Refusal::HeldAfterRestore`] with the current hold when
    /// `held` differs; nothing is confirmed. A store failure.
    pub async fn confirm_possibly_cancelled(
        &mut self,
        request: &crate::requests::RequestRef,
        held: &HeldRequest,
        _confirmed: pubky_switch_client::app::UserConfirmed,
    ) -> Result<()> {
        self.refresh_own()?;
        let target = crate::activity::ActivityTarget::payer(
            paykit_lib::derive::natural_key::request_record(&request.payee, &request.request_hash),
        );
        let record_key = self.activity_key(&target)?;
        self.activity_write_base(&record_key).await?;
        let Some(record) = self.held_activity(&target).await? else {
            return Err(self.request_record_refusal(&target).await?);
        };
        if let Some(key) = self.pass_hold(&record_key, &record, Some(held)).await? {
            self.local_put(&key::hold_confirmed(&key), &true).await?;
        }
        Ok(())
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
        let mut hold = self.local_get::<RestoreHold>(key::RESTORE_HOLD).await?;
        let held = hold.clone();
        if let Some(cursor) = self.local_get::<u64>(key::CURSOR_TIME).await? {
            if cursor > bound {
                tx.put(key::CURSOR_TIME, &bound)?;
                if let Some(hold) = hold.as_mut() {
                    hold.resend_from_ms = hold.resend_from_ms.min(resend_from(bound));
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
        // A missed item's window and a restore's first list read have independent effects
        // even when their start times coincide.
        let mut at_bound = None::<u64>;
        for (_, window) in self
            .local_scan::<crate::marks::BlockedWindow>(key::BLOCKED_WIDENINGS)
            .await?
        {
            let end = if window.until_ms == crate::marks::BlockedWindow::TO_NOW {
                window.until_ms
            } else {
                window.until_ms.min(bound)
            };
            if window.from_ms >= bound {
                if window.from_ms > bound {
                    tx.delete(&key::blocked_widening(window.from_ms));
                }
                at_bound = Some(at_bound.map_or(end, |previous| previous.max(end)));
            } else if end != window.until_ms {
                tx.put(
                    &key::blocked_widening(window.from_ms),
                    &crate::marks::BlockedWindow {
                        until_ms: end,
                        ..window
                    },
                )?;
            }
        }
        if let Some(until_ms) = at_bound {
            tx.put(
                &key::blocked_widening(bound),
                &crate::marks::BlockedWindow {
                    from_ms: bound,
                    until_ms,
                },
            )?;
        }
        if let Some(hold) = hold.as_mut() {
            hold.widening_from_ms = hold.widening_from_ms.map(|start| start.min(bound));
            hold.earlier_unread_from_ms = hold.earlier_unread_from_ms.map(|start| start.min(bound));
            if let Some(read) = hold.widening_read.as_mut() {
                read.at_ms = read.at_ms.min(bound);
            }
            if let Some(owed) = hold.earlier_widening.as_mut() {
                owed.from_ms = owed.from_ms.min(bound);
                owed.read.at_ms = owed.read.at_ms.min(bound);
            }
        }
        if hold != held {
            if let Some(hold) = &hold {
                tx.put(key::RESTORE_HOLD, hold)?;
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
        if self.app.list_collections().await? {
            // Switch keeps the listing position across calls and reloads.
            return Err(unlisted());
        }
        let mut tx = Tx::default();
        tx.delete(key::RECORDS_UNLISTED);
        tx.delete(key::MISSED_UNLISTED);
        tx.put(key::LISTED_CERT, &signed)?;
        self.local_commit(tx).await
    }

    /// Clear the restore's mark once the client finishes its owed listing with the scope
    /// keys held before and after the read. A read may use its page budget before then.
    /// A certificate taken during the read and a missed item received after its listing
    /// each require their own listing.
    pub(crate) async fn records_listed(&mut self, keys_held_before: bool) -> Result<()> {
        if !keys_held_before || !self.holds_scope_keys() {
            return Ok(());
        }
        if self
            .local_get::<bool>(key::RECORDS_UNLISTED)
            .await?
            .is_some()
            && !self.app.listing_owed().await?
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
        // The client and an explicit listing share a continuation. Finish the client's
        // owed listing before starting Paykit's own pass under the new certificate.
        if !owed || !self.holds_scope_keys() || self.app.listing_owed().await? {
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
    /// under `paykit_pay`, clears it. The backup doesn't hold the attempts made after it, and only
    /// that pass finds them on the payer's obligations (Paykit spec 7.3), so until then no
    /// new attempt is made and nothing is dispatched, whatever the host does meanwhile, such
    /// as [`Paykit::end_reconcile`] after a reconcile without `paykit_pay` skipped the pass.
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

    /// Deliver locally held answers once their record and counterparty permit it. After a
    /// restore, inspect the signed activity and Allowance messages for the host to reconcile.
    /// A shared record or an answer copied from a backup cannot establish that this
    /// installation still owes delivery (Paykit spec 7.2).
    pub(crate) async fn queue_own_again(&mut self) -> Result<()> {
        let owed = self
            .local_get::<OwnResendOwed>(key::OWN_RESEND_OWED)
            .await?;
        let mut held: std::collections::BTreeMap<String, HeldAnswer> = self
            .local_scan::<HeldAnswer>(key::HELD_ANSWERS)
            .await?
            .into_iter()
            .map(|(_, answer)| (answer.msg_id.clone(), answer))
            .collect();
        if owed.is_none() && held.is_empty() {
            return Ok(());
        }
        // An older installation did not distinguish a new held answer from an imported one.
        // A wallet that has ever restored cannot establish custody for such an answer.
        if held.values().any(|answer| answer.restored.is_none()) {
            let restored = self
                .local_get::<RestoreHold>(key::RESTORE_HOLD)
                .await?
                .is_some();
            for answer in held.values_mut() {
                answer.restored.get_or_insert(restored);
            }
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
                if record_held.different_write_at_held_version {
                    unread = true;
                    continue;
                }
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
                // restored-record scan judges every record again at the next receive, until 30
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
                if details_wait {
                    waiting.insert(counterparty);
                }
                for stored in messages {
                    let msg_id = &stored.message().msg_id;
                    let answer = held.get(msg_id);
                    if answer.is_none_or(|answer| answer.restored != Some(false)) {
                        self.note_restored_delivery(stored).await?;
                        if answer.is_some() {
                            done.insert(msg_id.clone());
                        }
                        continue;
                    }
                    if self.sends_held(&counterparty).await? {
                        still_held.insert(msg_id.clone());
                        continue;
                    }
                    done.insert(msg_id.clone());
                    match self.app.queue_stored(stored.plaintext()).await {
                        Ok(msg_id) => {
                            let _ = self.send_message(&msg_id).await;
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
        // Check an Allowance only after binding the record to its key (Paykit spec 7.2, 8.3).
        if let Some(restore) = owed.as_ref() {
            for record_held in Box::pin(self.app.records_of(Collection::Allowances.name())).await? {
                // As for an activity record: one under another policy holds nothing of
                // Paykit's and a delete holds no message, while one that doesn't open, read or
                // bind keeps the scan due on the next receive.
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
                for stored in messages {
                    self.note_restored_delivery(stored).await?;
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
                    if answer.restored == Some(false) {
                        given_up.push(answer.clone());
                    }
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
            // The restore's time, kept for the life of the mark. A mark without it, after a
            // restore that stopped before writing it, takes the time of the first pass that
            // finds a record it can't read, which is later: the wait never ends early.
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
        if held.different_write_at_held_version
            || matches!(held.content, RecordContent::Unreadable(_))
        {
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
    /// plaintext, and try to deliver it now ([`Paykit::send_owed_ends`]). One the client
    /// refuses for good, or as not this app's own, is neither queued nor told, and isn't
    /// asked for again; a store failure is the call's error.
    pub(crate) async fn queue_stored_again(
        &mut self,
        stored: &paykit_lib::records::StoredMessage,
    ) -> Result<()> {
        match self.app.queue_stored(stored.plaintext()).await {
            Ok(msg_id) => {
                let _ = self.send_message(&msg_id).await;
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

    /// Whether the restored-record scan has reached its bound for unreadable records.
    fn past_restore_window(&self, since_ms: u64) -> bool {
        self.window_now() > since_ms.saturating_add(pubky_switch_client::app::SENT_RETENTION_MS)
    }

    /// Write the restore time into the restored-record scan mark after the client's
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
        if self
            .activity_base_changed(&self.activity_key(&target)?)
            .await?
        {
            return Ok(DetailsStop::Unknown);
        }
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

    /// Queue, once after a restore whose verdict isn't clean, a `resend_request` to every
    /// counterparty with an open request or Allowance, from the restore's re-send time;
    /// [`Paykit::receive`] sends them. Nothing is asked for the restore while its verdict
    /// isn't known ([`RestoreHold::asks_due`]). The start is
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
        hold.resend_from_ms = hold.resend_from_ms.min(resend_from(bound));
        // The asks wait for the client's verdict on the restore: a clean one asks for
        // nothing, since no entry after the backup's position was passed, so none is lost
        // (Paykit spec 7.2); one that isn't clean, or a clean one a later report ends, asks.
        // An earlier restore's asks the backup owed go whatever this verdict.
        if !hold.asks_due() {
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

    /// A restore whose verdict Paykit hasn't taken yet counts as not clean, as an unknown
    /// verdict does: at the end of reconcile mode, after which the client may keep no
    /// verdict ([`Paykit::end_reconcile`]). Its asks and its widening of the block marks go
    /// then, the lists made due for the widening.
    pub(crate) async fn restore_verdict_not_known_holds(&mut self) -> Result<()> {
        let Some(mut hold) = self.local_get::<RestoreHold>(key::RESTORE_HOLD).await? else {
            return Ok(());
        };
        if hold.verdict != RestoreVerdict::Open {
            return Ok(());
        }
        hold.verdict = RestoreVerdict::NotClean;
        let mut tx = Tx::default();
        if hold.widening_from_ms.is_some() {
            tx.delete(key::LISTS_READ);
        }
        tx.put(key::RESTORE_HOLD, &hold)?;
        self.local_commit(tx).await
    }

    /// Apply the Switch's held cursor to this restore (Switch spec 12.4, Paykit spec 7.2).
    /// A confirmed clean page releases only this restore's hold and the consents it marked.
    /// An ahead or unknown verdict keeps the hold. If a later page ends a clean verdict,
    /// replace the confirmation epoch and require the re-sends and block window again.
    /// All changes from one verdict commit together. Returns whether the lists are due.
    pub(crate) async fn take_restore_verdict(&mut self) -> Result<bool> {
        let clean = match self.app.restore_held_cursor() {
            None => return Ok(false),
            Some(HeldCursor::Clean) if self.first_read_pending() => return Ok(false),
            Some(HeldCursor::Clean) => true,
            Some(_) => false,
        };
        let Some(mut hold) = self.local_get::<RestoreHold>(key::RESTORE_HOLD).await? else {
            return Ok(false);
        };
        let mut tx = Tx::default();
        let mut lists_due = false;
        match (hold.verdict, clean) {
            (RestoreVerdict::Open, true) => {
                let missed = self
                    .local_get::<bool>(key::MISSED_CONSENTS)
                    .await?
                    .is_some();
                let marked = Consent::Held {
                    confirm_again: true,
                };
                for (rest, _) in self.local_scan::<bool>(key::RESTORE_CONSENTS).await? {
                    let consent_key = format!("{}{rest}", key::CONSENTS);
                    let entry = self.local_get::<ConsentEntry>(&consent_key).await?;
                    if let Some(entry) = entry.filter(|entry| !missed && entry.consent == marked) {
                        let confirmed = Consent::Held {
                            confirm_again: false,
                        };
                        tx.put(&consent_key, &entry.with(confirmed))?;
                    }
                    tx.delete(&format!("{}{rest}", key::RESTORE_CONSENTS));
                }
                hold.verdict = RestoreVerdict::Clean;
            }
            (RestoreVerdict::Open, false) => {
                // The restore's widening of the block marks, which waited for the verdict, is
                // applied at the next read of the lists, made due now (`marks.rs`).
                if hold.widening_from_ms.is_some() {
                    tx.delete(key::LISTS_READ);
                    lists_due = true;
                }
                hold.verdict = RestoreVerdict::NotClean;
            }
            (RestoreVerdict::Clean, false) => {
                // A confirmation or re-send for an older gap cannot cover this new one.
                tx.put(key::HOLD_EPOCH, &HoldEpoch::new()?)?;
                hold.asks_queued = false;
                hold.asked.clear();
                for (rest, entry) in self.local_scan::<ConsentEntry>(key::CONSENTS).await? {
                    let again = entry.consent.confirm_again();
                    if again != entry.consent {
                        tx.put(&format!("{}{rest}", key::CONSENTS), &entry.with(again))?;
                    }
                }
                for prefix in [key::HOLD_CONFIRMED, key::HOLD_CONFIRMED_BEFORE] {
                    for (rest, _) in self.local_scan::<bool>(prefix).await? {
                        tx.delete(&format!("{prefix}{rest}"));
                    }
                }
                // Apply the window as of the first list read kept by this restore.
                if hold.widening_from_ms.is_some() {
                    tx.delete(key::LISTS_READ);
                    lists_due = true;
                }
                hold.verdict = RestoreVerdict::NotClean;
            }
            (RestoreVerdict::Clean, true) | (RestoreVerdict::NotClean, _) => return Ok(false),
        }
        if !clean {
            for (rest, _) in self.local_scan::<bool>(key::RESTORE_CONSENTS).await? {
                tx.delete(&format!("{}{rest}", key::RESTORE_CONSENTS));
            }
        }
        tx.put(key::RESTORE_HOLD, &hold)?;
        self.local_commit(tx).await?;
        Ok(lists_due)
    }
}

/// The restored-record scan (`p/ro`). Each restore starts a fresh scan. It finishes
/// once every readable record can be judged and the host's notices are durable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnResendOwed {
    /// `None` checks every counterparty, including after an unreadable record. Otherwise
    /// these counterparties, sorted and unique, have period details whose request record
    /// may still show a later stop ([`DetailsStop::Unknown`]).
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) waiting: Option<Vec<[u8; 32]>>,
    /// The restore time bounds retries of unreadable records. A restart before it was
    /// written takes the time of the first pass that needs the bound.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) restored_at_ms: Option<u64>,
}

/// Whether a later stop covers the period details a payee period record keeps, for the
/// restored-record scan ([`Paykit::queue_own_again`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DetailsStop {
    /// No stop is known for these details.
    Goes,
    /// The period is credited, or its request is rejected or cancelled: they aren't.
    Stopped,
    /// The request record can't show it yet: they wait, neither sent nor dropped, until it
    /// can or the client would refuse them by now anyway.
    Unknown,
}

/// This app's current and earlier period details while no response or cancellation closes them.
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
/// through a reload. A restore marks the answer as possibly delivered (Paykit spec 7.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeldAnswer {
    /// A backup carried this answer. Its signed record does not show its delivery outcome.
    #[serde(default)]
    pub(crate) restored: Option<bool>,
    /// The payee it goes to.
    #[serde(with = "pubky_switch_core::enc::serde_b64")]
    pub(crate) payee: [u8; 32],
    /// Its `msg_id`.
    pub(crate) msg_id: String,
    /// The request it answers.
    #[serde(with = "pubky_switch_core::enc::serde_b64")]
    pub(crate) request_hash: [u8; 32],
}

/// Outgoing messages whose disposition a shared activity record leaves open. A response,
/// cancellation or credited period can close an earlier message (Paykit spec 7.2). A
/// receipt can be stored without requesting delivery (Paykit wire 5.7), so its owner
/// must establish original delivery intent separately.
pub(crate) fn own_to_send_again<'a>(
    record: &'a ActivityRecord,
    own: &[u8; 32],
    app_id: &[u8; 16],
    details_go: bool,
) -> Vec<&'a paykit_lib::records::StoredMessage> {
    let answered = record.response.is_some()
        || record.cancellation.is_some()
        || record.proof.is_some()
        || record.receipt.is_some();
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

    fn hold_with_read(verdict: RestoreVerdict, from_ms: Option<u64>, at_ms: u64) -> RestoreHold {
        RestoreHold {
            verdict,
            widening_from_ms: from_ms,
            widening_read: Some(WideningRead {
                at_ms,
                deny: vec![[7; 32]],
            }),
            ..RestoreHold::of(10_000, None)
        }
    }

    #[test]
    fn test_restore_hold_first_read_is_bound_to_its_window() {
        assert!(RestoreHold::of(10_000, None).in_order());
        assert!(hold_with_read(RestoreVerdict::Clean, Some(5), 9).in_order());
        assert!(hold_with_read(RestoreVerdict::NotClean, Some(5), 5).in_order());
        assert!(hold_with_read(RestoreVerdict::Open, Some(5), 9).in_order());
        assert!(!hold_with_read(RestoreVerdict::Clean, Some(5), 4).in_order());
        assert!(!hold_with_read(RestoreVerdict::Clean, None, 9).in_order());
    }

    #[test]
    fn test_restore_hold_widening_owed_only_once_a_clean_verdict_ended() {
        let ended = hold_with_read(RestoreVerdict::NotClean, Some(5), 9);
        let owed = ended.widening_owed().unwrap();
        assert_eq!((owed.from_ms, owed.read.at_ms), (5, 9));
        assert!(hold_with_read(RestoreVerdict::Clean, Some(5), 9)
            .widening_owed()
            .is_none());
        assert!(RestoreHold::of(10_000, None).widening_owed().is_none());
    }

    #[test]
    fn test_restore_hold_of_carries_the_widenings_its_backup_owed() {
        let owed = |from_ms, at_ms, deny: [u8; 32]| OwedWidening {
            from_ms,
            read: WideningRead {
                at_ms,
                deny: vec![deny],
            },
        };
        // Its own owed widening, and none when the backup's hold owed none.
        let ended = hold_with_read(RestoreVerdict::NotClean, Some(5), 9);
        assert_eq!(
            RestoreHold::of(20_000, Some(&ended)).earlier_widening,
            Some(owed(5, 9, [7; 32]))
        );
        let clean = hold_with_read(RestoreVerdict::Clean, Some(5), 9);
        assert_eq!(RestoreHold::of(20_000, Some(&clean)).earlier_widening, None);
        // With one it carried too: one covering both.
        let both = RestoreHold {
            earlier_widening: Some(owed(3, 6, [8; 32])),
            ..ended
        };
        let covering = RestoreHold::of(20_000, Some(&both))
            .earlier_widening
            .unwrap();
        assert_eq!((covering.from_ms, covering.read.at_ms), (3, 9));
        assert_eq!(covering.read.deny, vec![[7; 32], [8; 32]]);
        // Out of order: read before its start.
        let reversed = RestoreHold {
            earlier_widening: Some(owed(9, 5, [8; 32])),
            ..RestoreHold::of(10_000, None)
        };
        assert!(!reversed.in_order());
    }

    fn extra<T: serde::de::DeserializeOwned + Serialize>(value: T) {
        let mut value = serde_json::to_value(value).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), serde_json::json!({"nested": [7]}));
        assert!(serde_json::from_value::<T>(value).is_err());
    }

    #[test]
    fn test_recovery_state_restore_hold_rejects_unknown() {
        extra(RestoreHold::of(300_000, None));
    }

    #[test]
    fn test_restore_hold_requires_its_block_window_members() {
        let hold = serde_json::to_value(RestoreHold::of(300_000, None)).unwrap();
        for name in [
            "widening_from_ms",
            "widening_read",
            "earlier_widening",
            "earlier_unread_from_ms",
        ] {
            let mut missing = hold.clone();
            missing.as_object_mut().unwrap().remove(name).unwrap();
            assert!(
                serde_json::from_value::<RestoreHold>(missing).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn test_recovery_state_restore_hold_round_trip() {
        let value = RestoreHold::of(300_000, None);
        let bytes = serde_json::to_vec(&value).unwrap();
        let read: RestoreHold = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(read, value);
    }

    #[test]
    fn test_recovery_state_missed_hold_rejects_unknown() {
        extra(MissedHold::bounded_by(300_000));
    }

    #[test]
    fn test_recovery_state_missed_hold_round_trip() {
        let value = MissedHold::bounded_by(300_000);
        let bytes = serde_json::to_vec(&value).unwrap();
        let read: MissedHold = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(read, value);
    }

    #[test]
    fn test_recovery_state_own_resend_rejects_unknown() {
        extra(OwnResendOwed::default());
    }

    #[test]
    fn test_recovery_state_own_resend_round_trip() {
        let value = OwnResendOwed::default();
        let bytes = serde_json::to_vec(&value).unwrap();
        let read: OwnResendOwed = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(read, value);
    }

    #[test]
    fn test_recovery_state_held_answer_rejects_unknown() {
        extra(HeldAnswer {
            restored: Some(false),
            payee: [7; 32],
            msg_id: "223e4567-e89b-42d3-a456-426614174000".into(),
            request_hash: [8; 32],
        });
    }

    #[test]
    fn test_recovery_state_held_answer_round_trip() {
        let value = HeldAnswer {
            restored: Some(false),
            payee: [7; 32],
            msg_id: "223e4567-e89b-42d3-a456-426614174000".into(),
            request_hash: [8; 32],
        };
        let bytes = serde_json::to_vec(&value).unwrap();
        let read: HeldAnswer = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(read, value);
    }

    #[test]
    fn test_recovery_state_own_resend_requires_waiting() {
        assert!(serde_json::from_value::<OwnResendOwed>(
            serde_json::json!({"restored_at_ms": null})
        )
        .is_err());
    }

    #[test]
    fn test_recovery_state_own_resend_requires_time() {
        assert!(
            serde_json::from_value::<OwnResendOwed>(serde_json::json!({"waiting": null})).is_err()
        );
    }

    #[test]
    fn test_recovery_state_own_resend_preserves_nullable_values() {
        for waiting in [None, Some(vec![]), Some(vec![[1; 32], [2; 32]])] {
            for restored_at_ms in [None, Some(300_000)] {
                let owed = OwnResendOwed {
                    waiting: waiting.clone(),
                    restored_at_ms,
                };
                let value = serde_json::to_value(&owed).unwrap();
                assert!(value.get("waiting").is_some());
                assert!(value.get("restored_at_ms").is_some());
                let read: OwnResendOwed = serde_json::from_value(value).unwrap();
                assert_eq!(read, owed);
            }
        }
    }

    #[test]
    fn test_recovery_state_duplicate_members_refused() {
        let hold = serde_json::to_string(&RestoreHold::of(0, None)).unwrap();
        let duplicate = hold.replacen('{', "{\"resend_from_ms\":1,", 1);
        assert!(serde_json::from_str::<RestoreHold>(&duplicate).is_err());
        assert!(serde_json::from_str::<MissedHold>(r#"{"bound_ms":1,"bound_ms":1}"#).is_err());
        assert!(serde_json::from_str::<OwnResendOwed>(
            r#"{"waiting":null,"waiting":null,"restored_at_ms":null}"#
        )
        .is_err());
        assert!(serde_json::from_str::<OwnResendOwed>(
            r#"{"waiting":null,"restored_at_ms":null,"restored_at_ms":null}"#
        )
        .is_err());
        let answer = HeldAnswer {
            restored: Some(false),
            payee: [7; 32],
            msg_id: "223e4567-e89b-42d3-a456-426614174000".into(),
            request_hash: [8; 32],
        };
        let text = serde_json::to_string(&answer).unwrap().replacen(
            "{",
            &format!("{{\"msg_id\":\"{}\",", answer.msg_id),
            1,
        );
        assert!(serde_json::from_str::<HeldAnswer>(&text).is_err());
    }

    #[test]
    fn test_recovery_state_wrong_types_refused() {
        let mut hold = serde_json::to_value(RestoreHold::of(0, None)).unwrap();
        hold["asks_queued"] = serde_json::json!("false");
        assert!(serde_json::from_value::<RestoreHold>(hold).is_err());
        assert!(
            serde_json::from_value::<MissedHold>(serde_json::json!({"bound_ms": null})).is_err()
        );
        assert!(serde_json::from_value::<OwnResendOwed>(serde_json::json!({
            "waiting": false, "restored_at_ms": null
        }))
        .is_err());
        assert!(serde_json::from_value::<OwnResendOwed>(serde_json::json!({
            "waiting": null, "restored_at_ms": "1"
        }))
        .is_err());
        let answer = HeldAnswer {
            restored: Some(false),
            payee: [7; 32],
            msg_id: "223e4567-e89b-42d3-a456-426614174000".into(),
            request_hash: [8; 32],
        };
        let mut value = serde_json::to_value(answer).unwrap();
        value["request_hash"] = serde_json::json!([]);
        assert!(serde_json::from_value::<HeldAnswer>(value).is_err());
    }

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
        let listed = cert(&["paykit_pay", "paykit_contacts", "paykit_request"], 0);
        for held in [
            // Narrower: one held between may have read less again.
            cert(&["paykit_pay", "paykit_contacts"], 1),
            // The same scopes, renewed: one held between may have read less.
            cert(&["paykit_pay", "paykit_contacts", "paykit_request"], 2),
            // Wider.
            cert(
                &[
                    "paykit_pay",
                    "paykit_contacts",
                    "paykit_request",
                    "paykit_allowance",
                ],
                3,
            ),
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
        let listed = cert(&["paykit_pay", "paykit_contacts", "paykit_request"], 0);
        for side in [RecordSide::Payer, RecordSide::Payee, RecordSide::All] {
            assert!(!side_unlisted(side, false, Some(&listed), &listed));
        }
        let payer_only = cert(&["paykit_pay", "paykit_contacts"], 1);
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
