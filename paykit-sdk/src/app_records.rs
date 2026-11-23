//! App records: Paykit's member of a Switch app record, `protocols.paykit` (Paykit spec 4,
//! Paykit wire 3.2).
//!
//! **Reading** a payee's public endpoints ([`Paykit::public_endpoints`], and a payment in
//! [`crate::PaymentMode::Public`]): the payee's `identity.json` is taken as for any
//! destination (Paykit spec 7.2; Switch spec 4.3), then the record of each app current in it,
//! in its order, is read from the payee's Homeserver, at the Switch's path for it (Switch spec
//! 4.2), and used only if:
//!
//! - it verifies as the Switch's app record for the payee (Switch wire 3.2), and its
//!   `app_id` is the one of the path it was read from;
//! - the app is current in the payee's `identity.json` with the certificate the record
//!   carries, which allows `paykit_request`, and a listed certificate hasn't expired (Paykit spec
//!   4);
//! - its Paykit member reads ([`PaykitMember::read`], Paykit wire 3.2);
//! - it is within its lifetime: before the earlier of any finite `expires_at` and its `updated_at`
//!   plus the identity's `record_max_age`, and its `updated_at` isn't ahead of the payer's
//!   clock (Paykit spec 4).
//!
//! Of one app's records, the one used is the one the Switch's comparison rules pick (Switch
//! spec 4.2, Switch wire 3.2 "Equal times"), through the core's `RecordView`: the newest
//! seen, so a record served that is older than one seen is answered from the newest seen,
//! checked as any record used. The records that took part are kept in the store, so the
//! comparison survives a restart; a refused record never displaces one kept.
//!
//! The record served now is judged on its own (Paykit spec 4): one refused, made ahead of
//! the payer's clock, or that can't be kept for the comparison is the app's failure, and the
//! app offers no endpoints in that read, not even those of an earlier record kept: the newer
//! record that can't be used may be the one that withdrew an endpoint. A conflict, two
//! bodies at one `updated_at` until a strictly later record ends it, is reported as the
//! app's failure too, and the reader keeps using the earlier record of the app it holds, if
//! any (Switch spec 4.2): that record's endpoints stay offered, judged as any record used,
//! and its own failure, if it has one, follows the conflict. Discovery
//! ([`Paykit::public_endpoints`]) and a public payment apply these same rules.
//!
//! A record that fails any of these is never paid. Each app's failure is that app's own
//! (Paykit spec 4): a record the Homeserver can't serve now, one the reader refuses,
//! or one that has expired is reported as that app's [`PublicAppFailure`], and the other
//! apps' usable endpoints stay available ([`PublicPaymentList`]). An app with no record, or
//! whose record offers no public endpoints, has none, and nothing of it fails. The read takes
//! at most [`MAX_PUBLIC_ENDPOINTS`] endpoints and [`MAX_PUBLIC_PAYLOAD_BYTES`] of payloads over
//! all the apps, in the order of `identity.json`: an app whose usable endpoints don't fit in
//! what is left is reported as over the bound, never cut to fit, and a later app still gets
//! what is left. A payment to a request's `required_app_id` reads only that app's record, so
//! that app's failure leaves it no public destination (Paykit spec 8.1); an app whose records
//! conflict still pays from the earlier record it holds, if any, the conflict reported
//! beside it (Switch spec 4.2). The same checks run
//! again before money moves, on the record kept as the destination's carrier.
//!
//! **Publishing** this app's own Paykit member is `crate::own_record`'s.
//!
//! Security: public endpoints are destinations anyone can read from the Homeserver, and so
//! anyone could plant a file there. Only a record signed by the app key the payee's
//! current certificate names, at that app's own path, is used, and only within the lifetime
//! the payee's signer bounds with `record_max_age`, so a stolen app key can't keep a
//! destination payable for long.

use paykit_lib::app_record::PaykitMember;
use paykit_lib::{PaymentEndpointIdentifier, PaymentEndpointPayload};
use std::ops::Range;
use std::sync::Arc;

use pubky_switch_client::app::{Initiator, PeerIdentity, SwitchCopy, UserConfirmed};
use pubky_switch_client::error::TransportError;
use pubky_switch_client::transport::BoxFuture;
use pubky_switch_core::enc::zbase32_encode;
use pubky_switch_core::records::{
    app_record_path, AppRecord, IdentityDoc, RecordOrder, RecordView, CLOCK_TOLERANCE_MS,
};
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};

use crate::destination::{
    current, homeserver_window, payee_copy_usable, public_read_window, Carrier, Clocks, Need,
};
use crate::error::{PaykitError, Refusal, Result};
use crate::journal::Destination;
use crate::paykit::Paykit;
use crate::state::key;
use paykit_lib::records::DestinationSource;

/// A public endpoint of a payee app, from its app record (Paykit spec 4).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PublicEndpoint {
    /// The payee app that published it.
    pub app_id: [u8; 16],
    /// The Payment Endpoint Identifier.
    pub endpoint_id: PaymentEndpointIdentifier,
    /// The payload, which only the rail interprets.
    pub payload: PaymentEndpointPayload,
}

/// The most public endpoints one read of a payee's public endpoints takes, over all its apps
/// (Paykit spec 4).
pub const MAX_PUBLIC_ENDPOINTS: usize = 256;

/// The most payload bytes, the sum of the payloads' lengths, one read of a payee's public
/// endpoints takes, over all its apps: 4 MiB (Paykit spec 4).
pub const MAX_PUBLIC_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// A payee's public Payment List as a payer reads it now (Paykit spec 4): the usable public
/// endpoints of its current apps, and the apps whose endpoints couldn't be used.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct PublicPaymentList {
    /// The public endpoints that may be paid now, app by app in the order of the payee's
    /// `identity.json`, then by Payment Endpoint Identifier.
    pub endpoints: Vec<PublicEndpoint>,
    /// The apps whose public endpoints couldn't be used, in the same order. An app with no
    /// record has no public endpoints and isn't one of them. An app here has no endpoints in
    /// [`Self::endpoints`], not even those of an earlier record the reader kept, except an app
    /// whose records conflict: the earlier record it holds, if any, is still used (Switch
    /// spec 4.2), and when that record fails too its failure follows the conflict (module
    /// docs).
    pub failures: Vec<PublicAppFailure>,
}

/// A payee app whose public endpoints couldn't be used in one read (Paykit spec 4): its
/// failure is its own, and the other apps' usable endpoints stay available.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PublicAppFailure {
    /// The payee app.
    pub app_id: [u8; 16],
    /// Why its endpoints couldn't be used.
    pub reason: PublicAppFailureReason,
}

/// Why a payee app's public endpoints couldn't be used (Paykit spec 4).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PublicAppFailureReason {
    /// The payee's Homeserver couldn't serve the app's record now. A later read may.
    Homeserver(TransportError),
    /// The reader refuses the record: it doesn't verify as the app's (Switch wire 3.2), its
    /// Paykit member doesn't read (Paykit wire 3.2), the app isn't current in the payee's
    /// `identity.json` with the record's certificate or that certificate doesn't allow
    /// `paykit_request`, the app's records conflict (Switch spec 4.2), or its newest records
    /// are more than the reader keeps of them. The text names the rule.
    Refused(String),
    /// The record has expired: past its `expires_at`, or its `updated_at` plus the identity's
    /// `record_max_age`, or made ahead of the payer's clock (Paykit spec 4). The text names
    /// the limit.
    Expired(String),
    /// The app's usable endpoints don't fit in what is left of [`MAX_PUBLIC_ENDPOINTS`] and
    /// [`MAX_PUBLIC_PAYLOAD_BYTES`] after the apps before it: none of them is taken.
    OverBound,
}

impl std::fmt::Display for PublicAppFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Homeserver(error) => write!(f, "homeserver: {error}"),
            Self::Refused(rule) => write!(f, "refused: {rule}"),
            Self::Expired(limit) => write!(f, "expired: {limit}"),
            Self::OverBound => write!(
                f,
                "over the bound of {MAX_PUBLIC_ENDPOINTS} endpoints and \
                 {MAX_PUBLIC_PAYLOAD_BYTES} payload bytes"
            ),
        }
    }
}

impl std::fmt::Display for PublicAppFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "app {}: {}", zbase32_encode(&self.app_id), self.reason)
    }
}

/// What is left of one read's bound ([`MAX_PUBLIC_ENDPOINTS`], [`MAX_PUBLIC_PAYLOAD_BYTES`]).
struct Room {
    endpoints: usize,
    payload_bytes: usize,
}

impl Room {
    fn new() -> Self {
        Self {
            endpoints: MAX_PUBLIC_ENDPOINTS,
            payload_bytes: MAX_PUBLIC_PAYLOAD_BYTES,
        }
    }

    /// Take `member`'s public endpoints whole when they fit in what is left; `false`, taking
    /// nothing, when they don't.
    fn take(&mut self, member: &PaykitMember) -> bool {
        let Some(left) = self.remaining_after(member) else {
            return false;
        };
        *self = left;
        true
    }

    fn remaining_after(&self, member: &PaykitMember) -> Option<Self> {
        let payload_bytes = member
            .public_endpoints
            .values()
            .map(|payload| payload.as_str().len())
            .sum::<usize>();
        Some(Self {
            endpoints: self.endpoints.checked_sub(member.public_endpoints.len())?,
            payload_bytes: self.payload_bytes.checked_sub(payload_bytes)?,
        })
    }
}

/// A payee app's record, verified and usable now, with its Paykit member.
pub(crate) struct UsableRecord {
    pub(crate) record: AppRecord,
    pub(crate) member: PaykitMember,
}

impl UsableRecord {
    /// The destination of its endpoint `id`.
    pub(crate) fn destination(
        &self,
        id: &PaymentEndpointIdentifier,
        payload: &PaymentEndpointPayload,
    ) -> Destination {
        Destination::new(
            self.record.body().app_id,
            id,
            payload,
            DestinationSource::Public,
            None,
            false,
        )
    }
}

/// A record read from `app_id`'s path on `payee`'s Homeserver, verified for the payee and
/// named by its path (Switch wire 3.2, Switch spec 4.2). `None` when it isn't: it is ignored.
pub(crate) fn read_record(payee: &[u8; 32], app_id: &[u8; 16], bytes: &[u8]) -> Option<AppRecord> {
    let signed: SignedDoc = serde_json::from_slice(bytes).ok()?;
    let record = AppRecord::verify(&signed, payee).ok()?;
    (&record.body().app_id == app_id).then_some(record)
}

/// Whether `record`, the record of `app_id` a reader uses, may be paid now (module docs).
/// `Err` when it isn't, naming why: it is never paid.
pub(crate) fn usable(
    app_id: &[u8; 16],
    record: &AppRecord,
    doc: &IdentityDoc,
    clocks: Clocks,
) -> std::result::Result<UsableRecord, PublicAppFailureReason> {
    let member = PaykitMember::read(record).map_err(|error| {
        PublicAppFailureReason::Refused(format!(
            "Paykit wire 3.2: the record's Paykit member doesn't read: {error}"
        ))
    })?;
    check_record_lifetime_and_certificate(app_id, record, doc, clocks)?;
    Ok(UsableRecord {
        record: record.clone(),
        member,
    })
}

fn check_record_lifetime_and_certificate(
    app_id: &[u8; 16],
    record: &AppRecord,
    doc: &IdentityDoc,
    clocks: Clocks,
) -> std::result::Result<(), PublicAppFailureReason> {
    let age_limit = record.trusted_until(doc.record_max_age_ms() / 1000);
    crate::destination::check_record_time(record, Some(age_limit), clocks)
        .map_err(PublicAppFailureReason::Expired)?;
    let carrier = Carrier::Record(Box::new(record.clone()));
    // The app's standing, with the record's certificate (the record age limit passed above).
    current(doc, app_id, &carrier, clocks).map_err(PublicAppFailureReason::Refused)
}

type AppRecordResult = std::result::Result<Option<Vec<u8>>, TransportError>;

/// Times of the original document and lookup, and the earliest and latest device
/// samples taken at either end of a GET. A pending lookup may finish on either path.
pub(crate) struct PublicRead {
    doc_hash: [u8; 32],
    lookup: Option<Range<u64>>,
    homeserver: Option<Range<u64>>,
    samples: Option<(u64, u64)>,
    /// Verified records with at least one usable endpoint, bounded by `MAX_PUBLIC_ENDPOINTS`.
    offered: Vec<[u8; 32]>,
}

struct RecordReads {
    observed: PublicRead,
    records: Vec<([u8; 16], AppRecordResult)>,
}

impl PublicRead {
    pub(crate) fn qualifies(&self, record: &AppRecord, identity: &PeerIdentity, now: u64) -> bool {
        self.current(identity, now)
            && record
                .signed()
                .doc_hash()
                .is_ok_and(|hash| self.offered.contains(&hash))
    }

    pub(crate) fn current(&self, identity: &PeerIdentity, now: u64) -> bool {
        if self.doc_hash != identity.doc.hash() {
            return false;
        }
        let (original, current) = match identity.switch_copy {
            SwitchCopy::Read => (&self.lookup, public_read_window(identity)),
            SwitchCopy::Unavailable | SwitchCopy::Pending | SwitchCopy::Stale => {
                (&self.homeserver, homeserver_window(identity))
            }
            _ => return false,
        };
        let (earliest, latest) = self.samples.unwrap_or((now, now));
        let covers =
            |w: &Range<u64>| w.contains(&earliest) && w.contains(&latest) && w.contains(&now);
        original.as_ref().is_some_and(covers) && current.as_ref().is_some_and(covers)
    }
}

/// How many records of one payee app are kept to rebuild its comparison state.
/// At a renewal a fortnight this is years of records. The oldest timestamp groups are
/// dropped together so compaction can only lose a fallback, never select half a conflict.
const SEEN_RECORDS: usize = 64;

/// The most bytes one payee app's kept records take, encoded: half the client store's cap
/// on one value ([`pubky_switch_client::store::MAX_VALUE_BYTES`]), so the kept value always
/// fits with room to spare. Past it the oldest are dropped as past [`SEEN_RECORDS`]; the
/// bound is per app, so one app's large records never crowd out another app's.
const SEEN_RECORDS_BYTES: usize = (pubky_switch_client::store::MAX_VALUE_BYTES / 2) as usize;

/// The records of one payee app a reader has seen that took part in its comparison (Switch
/// spec 4.2, Switch wire 3.2 "Equal times"), oldest first (`p/ar/<payee>/<app>`). Replayed
/// in order, they rebuild the core's [`RecordView`]; kept, they survive a restart, so a
/// copy older than one seen, or one conflicting with it, is never used after a reload.
///
/// A served record whose timestamp group is too large to keep leaves `unkept_at`, its
/// `updated_at`: no record at or before it is used, across restarts too, until a strictly
/// later record is kept (Switch spec 4.2).
///
/// Every record that leaves the set raises `dropped_at` to its `updated_at`
/// ([`SeenRecords::drop_before`], the one place a record leaves it), which nothing lowers or
/// clears. While it applies, a record at or before it, a whole group at exactly that time
/// included, is never kept, used, nor the fallback under a conflict: what was dropped may
/// have withdrawn it or conflicted with it, and the set no longer knows. Nothing is held for
/// good: a record newer than it is compared and used as usual, so a payee's newer record is
/// paid to as before.
///
/// The floor applies at a read only while it is no more than the clock tolerance ahead of
/// the reader's clock, as for `unkept_at`. It can be further ahead only when records the
/// payee dated ahead were read while the device clock ran fast and then dropped: until the
/// clock reaches it, a record dropped with them may be kept and used again, as without a
/// floor, and once the clock reaches it, such a record is not used. Every value a reader
/// writes holds the floor at or before its newest kept record or `unkept_at`, so a restored
/// value further ahead is refused ([`SeenRecords::verifies`]). Only public records are
/// compared here; a Private Payment List or a request's own destination never is.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SeenRecords {
    records: Vec<SignedDoc>,
    #[serde(deserialize_with = "Option::deserialize")]
    unkept_at: Option<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    dropped_at: Option<u64>,
}

impl SeenRecords {
    /// Where the kept part of `held`, in timestamp order, starts: past every record at or
    /// before the floor at `now` ([`SeenRecords::past_dropped`]), the newest complete
    /// timestamp groups that fit both bounds. Keeping just one side of a conflict could make
    /// it the fallback for a later conflict (Switch spec 4.2).
    fn kept_from(&self, held: &[AppRecord], now: Option<u64>) -> usize {
        let floor = held.partition_point(|r| !self.past_dropped(r, now));
        let mut count = 0;
        let mut bytes = 0usize;
        let mut start = held.len();
        for group in held
            .get(floor..)
            .unwrap_or_default()
            .chunk_by(|a, b| a.updated_at() == b.updated_at())
            .rev()
        {
            count += group.len();
            for item in group {
                let size = serde_json::to_vec(item.signed()).map_or(usize::MAX, |b| b.len() + 1);
                bytes = bytes.saturating_add(size);
            }
            if count > SEEN_RECORDS || bytes > SEEN_RECORDS_BYTES {
                break;
            }
            start -= group.len();
        }
        start
    }

    /// Drop the records of `held`, in timestamp order, before `start`, as
    /// [`SeenRecords::kept_from`] places it, and return the rest: the one place a record
    /// leaves the set, so the newest time dropped always raises `dropped_at`.
    fn drop_before(&mut self, mut held: Vec<AppRecord>, start: usize) -> Vec<AppRecord> {
        let start = start.min(held.len());
        if let Some(newest) = start.checked_sub(1).and_then(|i| held.get(i)) {
            self.dropped_at = self.dropped_at.max(Some(newest.updated_at()));
        }
        held.split_off(start)
    }

    /// Whether `record` is past the floor, so it may be kept and used: later than
    /// `dropped_at`, or, at a read at the reader's clock `now`, under a floor more than the
    /// clock tolerance ahead of it, which doesn't apply yet. A merge, with no clock, takes
    /// the floor as kept.
    fn past_dropped(&self, record: &AppRecord, now: Option<u64>) -> bool {
        self.dropped_at.is_none_or(|at| {
            record.updated_at() > at
                || now.is_some_and(|now| at > now.saturating_add(CLOCK_TOLERANCE_MS))
        })
    }

    /// Insert into timestamp order and keep the newest complete groups within both bounds.
    /// Return their view, or `None` if the served record itself would have to be dropped,
    /// which then drops nothing: so a record at or before a floor that applies is never kept.
    fn keep(
        &mut self,
        record: &AppRecord,
        mut held: Vec<AppRecord>,
        now: u64,
    ) -> Option<RecordView> {
        let at = held.partition_point(|r| r.updated_at() <= record.updated_at());
        held.insert(at, record.clone());
        let start = self.kept_from(&held, Some(now));
        if at < start {
            self.unkept_at = self.unkept_at.max(Some(record.updated_at()));
            return None;
        }
        let kept = self.drop_before(held, start);
        let view = Self::replay(&kept, now);
        self.records = kept.into_iter().map(SignedDoc::from).collect();
        if self.unkept_at.is_some_and(|at| record.updated_at() > at) {
            self.unkept_at = None;
        }
        Some(view)
    }

    /// Whether `record`, served now, is at or before a timestamp group the reader couldn't
    /// keep: its other bodies are unknown, so it may be in conflict or withdrawn. A group
    /// ahead of the reader's clock `now` is set aside, as a record ahead of it is (Switch
    /// wire 3.2): one is never kept from a record ahead of the clock, so this is only after
    /// the clock went back, or a restored value no record set, which then holds nothing.
    fn before_unkept(&self, served_at: Option<u64>, now: u64) -> bool {
        self.unkept_at.is_some_and(|at| {
            at <= now.saturating_add(CLOCK_TOLERANCE_MS)
                && served_at.is_none_or(|served| served <= at)
        })
    }

    /// Whether the kept records each are `payee`'s, verified as its signer signed it, and of
    /// app `app_id`, and there is one at least, a group the reader couldn't keep or a record
    /// dropped, with `dropped_at` at or before the newest kept record or `unkept_at`, as
    /// every value a reader writes holds it: checked
    /// before a restore installs a copy (`p/ar/<payee>/<app>`), so none can fail every later
    /// read of the payee. A group's time isn't signed, so one ahead of the clock holds
    /// nothing ([`SeenRecords::before_unkept`]).
    pub(crate) fn verifies(&self, payee: &[u8; 32], app_id: &[u8; 16]) -> bool {
        let mut newest = self.unkept_at;
        for signed in &self.records {
            match AppRecord::verify(signed, payee) {
                Ok(record) if record.cert().app_id() == *app_id => {
                    newest = newest.max(Some(record.updated_at()));
                }
                _ => return false,
            }
        }
        newest.is_some() && self.dropped_at <= newest
    }

    /// The union of these seen records and `other`'s, both of one app of `payee` (Switch spec
    /// 4.2). The later of the two `dropped_at` comes first: every record of either at or
    /// before it is dropped, so a record one side dropped never comes back through the
    /// other. Every other record of either, once, is then kept as a read keeps them
    /// ([`SeenRecords::kept_from`]), so a conflict either saw stays one, and what doesn't
    /// fit is dropped as at a read ([`SeenRecords::drop_before`]). The unkept bound is the
    /// later of the two, or the newest group's if none fits; a merge never lifts it, as a
    /// copy may hold a later record beside it once the clock went back, and the next record
    /// a read keeps past it lifts it as usual. `None` when a record doesn't verify as
    /// `payee`'s.
    pub(crate) fn merged(&self, other: &Self, payee: &[u8; 32]) -> Option<Self> {
        let mut merged = Self {
            records: Vec::new(),
            unkept_at: self.unkept_at.max(other.unkept_at),
            dropped_at: self.dropped_at.max(other.dropped_at),
        };
        let mut all: Vec<AppRecord> = Vec::new();
        let mut docs: Vec<Vec<u8>> = Vec::new();
        for signed in self.records.iter().chain(&other.records) {
            let record = AppRecord::verify(signed, payee).ok()?;
            let bytes = serde_json::to_vec(record.signed()).ok()?;
            if !docs.contains(&bytes) {
                docs.push(bytes);
                all.push(record);
            }
        }
        all.sort_by_key(AppRecord::updated_at);
        let start = merged.kept_from(&all, None);
        if let Some(newest) = all
            .last()
            .filter(|n| start == all.len() && merged.past_dropped(n, None))
        {
            merged.unkept_at = merged.unkept_at.max(Some(newest.updated_at()));
        }
        let kept = merged.drop_before(all, start);
        merged.records = kept.into_iter().map(SignedDoc::from).collect();
        Some(merged)
    }

    /// The view the records give, at the reader's clock `now`, and the records held. A
    /// record at or before a floor that applies at `now`, kept while it didn't, is held until
    /// the next record kept drops it, and plays no part in the view. A kept record that no
    /// longer verifies is a corrupt store.
    fn view(&self, payee: &[u8; 32], now: u64) -> Result<(RecordView, Vec<AppRecord>)> {
        let mut records = Vec::with_capacity(self.records.len());
        for signed in &self.records {
            let record = AppRecord::verify(signed, payee).map_err(|_| PaykitError::corrupt())?;
            records.push(record);
        }
        records.sort_by_key(AppRecord::updated_at);
        let from = records.partition_point(|r| !self.past_dropped(r, Some(now)));
        let view = Self::replay(records.get(from..).unwrap_or_default(), now);
        Ok((view, records))
    }

    fn replay(records: &[AppRecord], now: u64) -> RecordView {
        let mut view = RecordView::new();
        for record in records {
            view.see(record, now);
        }
        view
    }
}

/// Whether `record` can change a view that has seen `held`: one ahead of the clock takes part
/// in nothing, one seen already is the same record, and one earlier than a record seen is
/// earlier than the newest, which the view never goes back from (Switch wire 3.2).
/// A record conflicting with the newest while the view is in conflict already adds nothing:
/// one more body at that `updated_at` keeps the conflict as it is. It is compared only with
/// the records seen that the view uses at `now`: one ahead of the clock, as after the payer's
/// clock went back, is set aside by the view, so a record served earlier than it but later
/// than the rest, such as a withdrawal, takes part and is used, not the record before it.
fn takes_part(record: &AppRecord, held: &[AppRecord], view: &RecordView, now: u64) -> bool {
    let adds_nothing = |h: &AppRecord| match record.order(h) {
        RecordOrder::Same | RecordOrder::Earlier => true,
        RecordOrder::Conflict => view.in_conflict(),
        RecordOrder::Later => false,
    };
    !record.is_ahead(now) && !held.iter().filter(|h| !h.is_ahead(now)).any(adds_nothing)
}

impl Paykit {
    /// The payee's app records that may be used now (module docs), of `app` only when given,
    /// and the apps whose records couldn't be used: in the order of the payee's
    /// `identity.json`, within the read's bound. `Err` inside holds the refusal of the payee's
    /// `identity.json`. Each app whose records couldn't be used goes into `failures` as it
    /// is judged, so an error later in the read leaves the failures found before it there.
    /// The rules of discovery ([`Paykit::public_endpoints`]) and of a public
    /// payment alike: an app whose record served now fails gives its failure and no endpoints
    /// (Paykit spec 4), except a conflict, reported while an earlier record held is still
    /// used (Switch spec 4.2).
    pub(crate) async fn public_records(
        &mut self,
        payee: &[u8; 32],
        app: Option<[u8; 16]>,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
        failures: &mut Vec<PublicAppFailure>,
    ) -> Result<std::result::Result<(PeerIdentity, Vec<UsableRecord>), Refusal>> {
        // The current apps' records are read from the Homeserver beside the payee's switch
        // lookup, from the copy of identity.json taken first (Paykit spec 4, 14).
        let homeserver = Arc::clone(&self.homeserver);
        let clock = Arc::clone(&self.clock);
        let owner = *payee;
        let read_records = |identity: &PeerIdentity| -> BoxFuture<'static, RecordReads> {
            let doc_hash = identity.doc.hash();
            let lookup = public_read_window(identity);
            let homeserver_observation = homeserver_window(identity);
            let apps: Vec<_> = identity
                .doc
                .body()
                .apps
                .iter()
                .map(|a| a.app_id)
                .filter(|id| app.is_none_or(|a| a == *id))
                .collect();
            let homeserver = Arc::clone(&homeserver);
            let clock = Arc::clone(&clock);
            Box::pin(async move {
                // A clock correction during one GET cannot qualify bytes read outside
                // the original identity's window. Keep the pass only if every read fits.
                let mut samples = None::<(u64, u64)>;
                let mut out = Vec::with_capacity(apps.len());
                for app_id in apps {
                    let started = clock.now_ms();
                    let bytes = homeserver.get(owner, app_record_path(&app_id)).await;
                    let ended = clock.now_ms();
                    let (min, max) = samples.unwrap_or((started, started));
                    samples = Some((min.min(started).min(ended), max.max(started).max(ended)));
                    out.push((app_id, bytes));
                }
                RecordReads {
                    observed: PublicRead {
                        doc_hash,
                        lookup,
                        homeserver: homeserver_observation,
                        samples,
                        offered: Vec::new(),
                    },
                    records: out,
                }
            })
        };
        let (identity, reads) = self
            .payee_identity_with(payee, Need::SwitchCopy, by, confirmed, |identity| {
                Some(read_records(identity))
            })
            .await?;
        let mut identity = match identity {
            Ok(identity) => identity,
            Err(refusal) => return Ok(Err(refusal)),
        };
        // The first reads belong to that signed document while fresh. A later copy can
        // close a renewal even when it still lists the same App ID and certificate.
        let mut first_reads = reads.filter(|reads| reads.observed.current(&identity, self.now()));
        const MAX_REFRESHES: usize = 3;
        let mut refreshes = 0;
        loop {
            let fetched = match first_reads.take() {
                Some(records) => records,
                None => read_records(&identity).await,
            };
            if let Some(answered) = match self.take_kept_lookup(payee).await? {
                Ok(answered) => answered,
                Err(refusal) => return Ok(Err(refusal)),
            } {
                identity = answered;
            }
            let now = self.now();
            if fetched.observed.current(&identity, now)
                && payee_copy_usable(now, &identity, Need::SwitchCopy, by, confirmed)
            {
                let start = failures.len();
                let records = self
                    .judge_public_records(payee, &identity.doc, now, fetched.records, failures)
                    .await?;
                let mut observed = fetched.observed;
                if let Some(answered) = match self.take_kept_lookup(payee).await? {
                    Ok(answered) => answered,
                    Err(refusal) => return Ok(Err(refusal)),
                } {
                    identity = answered;
                }
                let used_at = self.now();
                if observed.current(&identity, used_at)
                    && payee_copy_usable(used_at, &identity, Need::SwitchCopy, by, confirmed)
                {
                    observed.offered = records
                        .iter()
                        .map(|r| r.record.signed().doc_hash())
                        .collect::<std::result::Result<_, _>>()?;
                    self.last_public_read = Some(observed);
                    return Ok(Ok((identity, records)));
                }
                failures.truncate(start);
            }
            if refreshes == MAX_REFRESHES {
                return Ok(Err(Refusal::IdentityUnavailable(
                    "Switch spec 4.3: the payee's identity expired during its app record reads"
                        .into(),
                )));
            }
            refreshes += 1;
            identity = match self
                .payee_identity(payee, Need::SwitchCopy, by, confirmed)
                .await?
            {
                Ok(identity) => identity,
                Err(refusal) => return Ok(Err(refusal)),
            };
        }
    }

    async fn judge_public_records(
        &mut self,
        payee: &[u8; 32],
        doc: &IdentityDoc,
        now: u64,
        fetched: Vec<([u8; 16], AppRecordResult)>,
        failures: &mut Vec<PublicAppFailure>,
    ) -> Result<Vec<UsableRecord>> {
        // A record's expiry is judged by both clocks (Paykit spec 7.2).
        let clocks = Clocks {
            device: now,
            service: self.service_time(),
        };
        let mut usable_records = Vec::new();
        let mut room = Room::new();
        for (app_id, bytes) in fetched {
            let fail = |reason| PublicAppFailure { app_id, reason };
            // One app's Homeserver read failing is that app's failure (Paykit spec 4).
            let bytes = match bytes {
                Ok(bytes) => bytes,
                Err(error) => {
                    failures.push(fail(PublicAppFailureReason::Homeserver(error)));
                    continue;
                }
            };
            let read = bytes.as_ref().and_then(|b| read_record(payee, &app_id, b));
            // The record served is compared with those seen before (Switch spec 4.2): an older
            // one never replaces a newer one, and two bodies at one `updated_at` are a
            // conflict, during which neither is used and the reader goes on with the earlier
            // record it holds, until a strictly later one ends it.
            let seen_key = key::seen_records(payee, &app_id);
            let mut seen = self
                .local_get::<SeenRecords>(&seen_key)
                .await?
                .unwrap_or_default();
            let (mut view, held) = seen.view(payee, now)?;
            // An app with no record has no public endpoints, whatever was seen before, and
            // nothing of it fails (Paykit spec 4).
            if bytes.is_none() {
                continue;
            }
            // Every record that verifies as the app's takes part in the comparison, whatever
            // its Paykit member: two bodies at one `updated_at` are a conflict for every reader
            // (Switch wire 3.2), and the newest record is the app's (Switch spec 4.2). A Paykit
            // member that doesn't read makes that record's endpoints unusable, as `usable`
            // judges the record used (Paykit wire 3.2), and decides nothing of the comparison.
            let served_ahead = read.as_ref().is_some_and(|record| record.is_ahead(now));
            let served_at = read.as_ref().map(AppRecord::updated_at);
            let served_refused = read.is_none();
            if let Some(record) = read {
                // A record at or before one dropped from the set takes part in nothing, as a
                // record earlier than one held: what was dropped may have withdrawn it or
                // conflicted with it (Switch spec 4.2). The records held answer for it.
                if seen.past_dropped(&record, Some(now)) && takes_part(&record, &held, &view, now) {
                    // A record the reader can't keep for the comparison is never used, nor
                    // is an earlier one in its place, which it may withdraw: the app's
                    // failure, and the other apps' reads go on (Paykit spec 4, below).
                    let Some(kept) = seen.keep(&record, held, now) else {
                        // Kept before the failure is told: after a restart the records
                        // retained alone could make an earlier one look current.
                        self.local_put(&seen_key, &seen).await?;
                        // Report the read's bound for otherwise usable endpoints. Compare
                        // the record without retaining it or taking room from later apps.
                        let over_bound = PaykitMember::read(&record)
                            .is_ok_and(|member| room.remaining_after(&member).is_none())
                            && check_record_lifetime_and_certificate(&app_id, &record, doc, clocks)
                                .is_ok()
                            && !view.see(&record, now);
                        let reason = if over_bound {
                            PublicAppFailureReason::OverBound
                        } else {
                            PublicAppFailureReason::Refused(format!(
                                "Paykit spec 4: the served record exceeds the history a reader keeps \
                                 for this app ({SEEN_RECORDS} records, {SEEN_RECORDS_BYTES} bytes)"
                            ))
                        };
                        failures.push(fail(reason));
                        continue;
                    };
                    view = kept;
                    self.local_put(&seen_key, &seen).await?;
                }
            }
            // A record at or before a group the reader couldn't keep is never used, nor an
            // earlier one in its place, until a strictly later record is kept.
            if seen.before_unkept(served_at, now) {
                failures.push(fail(PublicAppFailureReason::Refused(format!(
                    "Paykit spec 4: the app's records up to a group that exceeded the history \
                     a reader keeps ({SEEN_RECORDS} records, {SEEN_RECORDS_BYTES} bytes) are \
                     not used"
                ))));
                continue;
            }
            // The record served now is judged on its own: refused, made ahead of the payer's
            // clock, or in conflict with one seen, it is reported as this app's failure.
            let served_failure = if served_refused {
                Some(PublicAppFailureReason::Refused(
                    "Switch wire 3.2: the record served doesn't verify as the app's".into(),
                ))
            } else if served_ahead {
                Some(PublicAppFailureReason::Expired(
                    "Paykit spec 4: the record was made ahead of the payer's clock".into(),
                ))
            } else if view.in_conflict() {
                Some(PublicAppFailureReason::Refused(
                    "Switch spec 4.2: the app's records conflict at one updated_at".into(),
                ))
            } else if view.used().is_none() {
                // The view sets aside the records seen that are ahead of the clock, as after
                // the payer's clock went back; the record served is older than one of them,
                // so nothing is used until the clock passes them.
                Some(PublicAppFailureReason::Expired(
                    "Paykit spec 4: the app's newest record seen was made ahead of the \
                     payer's clock"
                        .into(),
                ))
            } else {
                None
            };
            // A conflict is told, and the reader "keeps using an earlier record of that app it
            // already holds, if any" (Switch spec 4.2): that record, the one the view uses, is
            // judged as any record used. Paykit spec 4 governs every other failure of the
            // record served: "One app's failure, a record it can't read, or one it refuses or
            // that has expired, is reported as that app's, and the other apps' usable
            // endpoints stay available"; "a request with `required_app_id` reads only that
            // app's record, so that app's failure leaves the request with no public
            // destination". So such an app offers nothing in this read, not even an earlier
            // record kept, which only the comparison uses; nor does an app in conflict with
            // no earlier record.
            let in_conflict = !served_refused && !served_ahead && view.in_conflict();
            if let Some(reason) = served_failure {
                failures.push(fail(reason));
                if !(in_conflict && view.used().is_some()) {
                    continue;
                }
            }
            let Some(used) = view.used() else {
                continue;
            };
            // A record without public endpoints offers none, and nothing of it fails.
            if PaykitMember::read(used).is_ok_and(|member| member.public_endpoints.is_empty()) {
                continue;
            }
            match usable(&app_id, used, doc, clocks) {
                // An app's endpoints are taken whole or not at all (Paykit spec 4).
                Ok(usable) if room.take(&usable.member) => usable_records.push(usable),
                Ok(_) => failures.push(fail(PublicAppFailureReason::OverBound)),
                Err(reason) => failures.push(fail(reason)),
            }
        }
        Ok(usable_records)
    }

    /// The payee's public Payment List (Paykit spec 4): the public endpoints of `payee`'s
    /// current apps that may be paid now, read from their records on the payee's Homeserver
    /// and checked as the module docs say, and each app whose endpoints couldn't be used,
    /// with why: its record unreadable now, refused, expired, or over the read's bound. One
    /// app's failure never fails the read, and a failed app offers no endpoints in it, not
    /// even those of an earlier record kept, except an app whose records conflict, which
    /// keeps the earlier record it holds, if any (module docs); a public payment reads by the
    /// same rules. `confirmed` is the user's confirmation for a payee whose switch couldn't
    /// confirm its apps ([`Refusal::IdentityUnconfirmed`]).
    ///
    /// # Errors
    ///
    /// [`Refusal::IdentityUnavailable`] or [`Refusal::IdentityUnconfirmed`] for the payee's
    /// `identity.json`, including when repeated record reads outlast each fresh identity;
    /// [`Refusal::CertificateRefused`] under a certificate Paykit refuses.
    pub async fn public_endpoints(
        &mut self,
        payee: &[u8; 32],
        confirmed: Option<UserConfirmed>,
    ) -> Result<PublicPaymentList> {
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.public_endpoints_inner(payee, confirmed)).await
    }

    async fn public_endpoints_inner(
        &mut self,
        payee: &[u8; 32],
        confirmed: Option<UserConfirmed>,
    ) -> Result<PublicPaymentList> {
        self.refresh_own()?;
        let mut failures = Vec::new();
        let (_, records) = self
            .public_records(payee, None, Initiator::User, confirmed, &mut failures)
            .await??;
        let endpoints = records
            .iter()
            .flat_map(|usable| {
                usable
                    .member
                    .public_endpoints
                    .iter()
                    .map(|(id, payload)| PublicEndpoint {
                        app_id: usable.record.body().app_id,
                        endpoint_id: id.clone(),
                        payload: payload.clone(),
                    })
            })
            .collect();
        Ok(PublicPaymentList {
            endpoints,
            failures,
        })
    }
}

#[cfg(test)]
mod tests {
    use pubky_switch_core::records::AppRecordBody;
    use serde_json::json;

    use super::*;
    use crate::destination::carries;
    use crate::testkit::{object, Party, NOW};

    /// The device clock at `now`, with no service time verified.
    fn device(now: u64) -> Clocks {
        Clocks {
            device: now,
            service: None,
        }
    }

    /// A record served at `app_id`'s path, read and checked as one used.
    fn usable_bytes(
        payee: &[u8; 32],
        app_id: &[u8; 16],
        bytes: &[u8],
        doc: &IdentityDoc,
        now: u64,
    ) -> Option<UsableRecord> {
        usable(
            app_id,
            &read_record(payee, app_id, bytes)?,
            doc,
            device(now),
        )
        .ok()
    }

    const DAY_MS: u64 = 86_400_000;

    /// `app`'s record with `member` as its Paykit member, made at `updated_at` and expiring
    /// at `expires_at`, as the Homeserver serves it.
    fn record_bytes(
        app: &Party,
        member: serde_json::Value,
        updated_at: u64,
        expires_at: Option<u64>,
    ) -> Vec<u8> {
        let mut body =
            AppRecordBody::new(&app.cert, vec!["paykit.payment_request".into()], updated_at);
        body.protocols.insert("paykit".into(), object(member));
        body.expires_at = expires_at;
        let record = AppRecord::sign(&app.keys, body).unwrap();
        serde_json::to_vec(record.signed()).unwrap()
    }

    fn tip_jar() -> serde_json::Value {
        json!({ "public_endpoints": { "btc-lightning-bolt12": "lno1tips" } })
    }

    #[test]
    fn test_app_records_current_record_is_usable_and_carries_its_endpoint() {
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let doc = bob.doc(&[&bob]);
        let when = NOW + 31 * DAY_MS;
        let bytes = record_bytes(&bob, tip_jar(), NOW, None);
        let usable = usable_bytes(&bob.id(), &bob.app_id(), &bytes, &doc, when).unwrap();
        let (id, payload) = usable.member.public_endpoints.iter().next().unwrap();
        let destination = usable.destination(id, payload);
        let carrier = Carrier::Record(Box::new(usable.record));
        assert_eq!(
            carries(&bob.id(), &destination, &carrier, None, device(when)),
            Ok(())
        );
        // Another payload isn't carried.
        let mut other = destination.clone();
        other.payload = "lno1other".into();
        assert!(carries(&bob.id(), &other, &carrier, None, device(when)).is_err());
    }

    #[test]
    fn test_app_records_malformed_or_misplaced_record_ignored() {
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let doc = bob.doc(&[&bob]);
        let later = NOW + 30 * DAY_MS;
        // A payload that isn't a string (Paykit wire 3.2).
        let bad = record_bytes(
            &bob,
            json!({ "public_endpoints": { "btc-lightning-bolt12": 7 } }),
            NOW,
            Some(later),
        );
        assert!(usable_bytes(&bob.id(), &bob.app_id(), &bad, &doc, NOW + 1).is_none());
        // Bytes that aren't a signed record, and a record read from another app's path.
        assert!(usable_bytes(&bob.id(), &bob.app_id(), b"{}", &doc, NOW + 1).is_none());
        let good = record_bytes(&bob, tip_jar(), NOW, Some(later));
        assert!(usable_bytes(&bob.id(), &[9; 16], &good, &doc, NOW + 1).is_none());
        // Another identity's record.
        let carol = Party::new(
            "carol",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        assert!(usable_bytes(&carol.id(), &bob.app_id(), &good, &doc, NOW + 1).is_none());
    }

    #[test]
    fn test_app_records_expired_stale_or_ahead_record_ignored() {
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let doc = bob.doc(&[&bob]);
        // Expired at its own expires_at.
        let bytes = record_bytes(&bob, tip_jar(), NOW, Some(NOW + DAY_MS));
        assert!(usable_bytes(&bob.id(), &bob.app_id(), &bytes, &doc, NOW + DAY_MS - 1).is_some());
        assert!(usable_bytes(&bob.id(), &bob.app_id(), &bytes, &doc, NOW + DAY_MS).is_none());
        // Past updated_at plus the identity's record age limit (90 days when absent), whatever
        // expires_at it signed.
        let far = record_bytes(&bob, tip_jar(), NOW, Some(NOW + 365 * DAY_MS));
        assert!(usable_bytes(&bob.id(), &bob.app_id(), &far, &doc, NOW + 89 * DAY_MS).is_some());
        assert!(usable_bytes(&bob.id(), &bob.app_id(), &far, &doc, NOW + 90 * DAY_MS).is_none());
        // The same identity limit also bounds a signed record with no deadline.
        let open = record_bytes(&bob, tip_jar(), NOW, None);
        assert!(usable_bytes(&bob.id(), &bob.app_id(), &open, &doc, NOW + 31 * DAY_MS).is_some());
        assert!(usable_bytes(&bob.id(), &bob.app_id(), &open, &doc, NOW + 90 * DAY_MS).is_none());
        // Made more than 5 minutes ahead of the payer's clock.
        let ahead = record_bytes(&bob, tip_jar(), NOW + 10 * 60_000, Some(NOW + DAY_MS));
        assert!(usable_bytes(&bob.id(), &bob.app_id(), &ahead, &doc, NOW).is_none());
    }

    #[test]
    fn test_app_records_record_with_wrong_permissions_ignored() {
        use pubky_switch_core::scopes::Permission;
        // The Switch takes both certificates; Paykit's verifiers refuse them (Paykit wire
        // 3.1): `paykit_request` needs exactly `conversation.add`.
        let missing = Party::with_permissions(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
            &[],
        );
        let extra = Party::with_permissions(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
            &[
                Permission::ClaimsRead,
                Permission::ListsRead,
                Permission::ConversationAdd,
            ],
        );
        for app in [&missing, &extra] {
            let doc = app.doc(&[app]);
            let bytes = record_bytes(app, tip_jar(), NOW, Some(NOW + DAY_MS));
            assert!(usable_bytes(&app.id(), &app.app_id(), &bytes, &doc, NOW + 1).is_none());
        }
    }

    #[test]
    fn test_app_records_record_of_an_app_not_current_ignored() {
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let bytes = record_bytes(&bob, tip_jar(), NOW, Some(NOW + DAY_MS));
        assert!(usable_bytes(&bob.id(), &bob.app_id(), &bytes, &bob.doc(&[]), NOW + 1).is_none());
        // An app without `paykit_request` publishes no destination.
        let wallet = Party::new("bob", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bytes = record_bytes(&wallet, tip_jar(), NOW, Some(NOW + DAY_MS));
        let doc = bob.doc(&[&wallet]);
        assert!(usable_bytes(&bob.id(), &wallet.app_id(), &bytes, &doc, NOW + 1).is_none());
    }

    fn discovery_records() -> (Party, SeenRecords) {
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let bytes = record_bytes(&bob, tip_jar(), NOW, Some(NOW + DAY_MS));
        let record =
            AppRecord::verify(&serde_json::from_slice(&bytes).unwrap(), &bob.id()).unwrap();
        let mut records = SeenRecords::default();
        records.keep(&record, Vec::new(), NOW).unwrap();
        (bob, records)
    }

    #[test]
    fn test_app_records_replay_timestamp_groups_from_stored_observations() {
        let (bob, _) = discovery_records();
        let signed = |ms, member| {
            serde_json::from_slice::<SignedDoc>(&record_bytes(
                &bob,
                member,
                NOW + ms,
                Some(NOW + DAY_MS),
            ))
            .unwrap()
        };
        let mut records = SeenRecords {
            dropped_at: None,
            records: vec![
                signed(0, tip_jar()),
                signed(2, tip_jar()),
                signed(1, json!({})),
                signed(2, json!({})),
            ],
            unkept_at: None,
        };
        let (view, held) = records.view(&bob.id(), NOW + 2).unwrap();
        assert!(view.in_conflict());
        assert_eq!(view.used().unwrap().updated_at(), NOW + 1);

        let conflicting = AppRecord::verify(&signed(1, tip_jar()), &bob.id()).unwrap();
        records.keep(&conflicting, held, NOW + 1).unwrap();
        let times: Vec<_> = records
            .records
            .iter()
            .map(|doc| AppRecord::verify(doc, &bob.id()).unwrap().updated_at() - NOW)
            .collect();
        assert_eq!(times, [0, 1, 1, 2, 2]);
    }

    #[test]
    fn test_app_records_compaction_drops_whole_conflicting_timestamp() {
        let (bob, mut seen) = discovery_records();
        let make = |i, member| {
            let bytes = record_bytes(&bob, member, NOW + i, Some(NOW + DAY_MS));
            read_record(&bob.id(), &bob.app_id(), &bytes).unwrap()
        };
        let mut held = vec![make(0, tip_jar()), make(0, json!({}))];
        for i in 1..(SEEN_RECORDS - 1) {
            held.push(make(i as u64, tip_jar()));
        }
        seen.keep(&make(SEEN_RECORDS as u64, tip_jar()), held, NOW + DAY_MS)
            .unwrap();
        let (_, kept) = seen.view(&bob.id(), NOW + DAY_MS).unwrap();
        assert_eq!(kept.len(), SEEN_RECORDS - 1);
        assert_eq!(kept.first().unwrap().updated_at(), NOW + 1);
        // The dropped group raises the floor to its time.
        assert_eq!(seen.dropped_at, Some(NOW));
    }

    #[test]
    fn test_app_records_merged_keeps_both_sides_and_the_unkept_bound() {
        let (bob, _) = discovery_records();
        let signed = |ms, member| {
            serde_json::from_slice::<SignedDoc>(&record_bytes(
                &bob,
                member,
                NOW + ms,
                Some(NOW + DAY_MS),
            ))
            .unwrap()
        };
        let times = |seen: &SeenRecords| -> Vec<u64> {
            let mut at: Vec<_> = seen
                .records
                .iter()
                .map(|doc| AppRecord::verify(doc, &bob.id()).unwrap().updated_at() - NOW)
                .collect();
            at.sort_unstable();
            at
        };
        let seen = |records, unkept_at| SeenRecords {
            records,
            unkept_at,
            dropped_at: None,
        };
        // Each side of a conflict, and a record both hold, kept once.
        let first = seen(vec![signed(0, tip_jar()), signed(1, tip_jar())], None);
        let second = seen(vec![signed(0, tip_jar()), signed(1, json!({}))], None);
        let merged = first.merged(&second, &bob.id()).unwrap();
        assert_eq!(times(&merged), [0, 1, 1]);
        assert!(merged.view(&bob.id(), NOW + 1).unwrap().0.in_conflict());
        assert!(!first.view(&bob.id(), NOW + 1).unwrap().0.in_conflict());

        // A group one side couldn't keep holds the older records the other side has.
        let held = seen(Vec::new(), Some(NOW + 5));
        let merged = held.merged(&first, &bob.id()).unwrap();
        assert_eq!(merged.unkept_at, Some(NOW + 5));
        assert_eq!(times(&merged), [0, 1]);
        // A merge never lifts it, also beside a later record, which stays kept.
        let later = seen(vec![signed(6, tip_jar())], None);
        let merged = held.merged(&later, &bob.id()).unwrap();
        assert_eq!(merged.unkept_at, Some(NOW + 5));
        assert_eq!(times(&merged), [6]);
        // A group too large to keep leaves its time alone.
        let crowd = seen(
            (0..=SEEN_RECORDS)
                .map(|i| signed(9, json!({ "n": i })))
                .collect(),
            None,
        );
        let merged = crowd.merged(&first, &bob.id()).unwrap();
        assert_eq!(merged.unkept_at, Some(NOW + 9));
        assert_eq!(merged.dropped_at, Some(NOW + 9));
        assert!(times(&merged).is_empty());

        // A record of another identity refuses the merge.
        assert!(first.merged(&second, &[9; 32]).is_none());
    }

    #[test]
    fn test_app_records_floor_keeps_a_dropped_record_out_of_a_merge_and_a_read() {
        let (bob, _) = discovery_records();
        let signed = |ms, member| {
            serde_json::from_slice::<SignedDoc>(&record_bytes(
                &bob,
                member,
                NOW + ms,
                Some(NOW + DAY_MS),
            ))
            .unwrap()
        };
        let record = |doc: &SignedDoc| AppRecord::verify(doc, &bob.id()).unwrap();
        let seen = |records, dropped_at| SeenRecords {
            records,
            unkept_at: None,
            dropped_at,
        };
        // One side holds E0's record; the other dropped the withdrawal after it and holds a
        // conflict. Either way round, the merge never brings E0's record back as the
        // fallback under the conflict.
        let withdrawn = seen(vec![signed(0, tip_jar())], None);
        let conflicted = seen(
            vec![signed(2, json!({})), signed(2, json!({ "x": 1 }))],
            Some(NOW + 1),
        );
        for merged in [
            withdrawn.merged(&conflicted, &bob.id()).unwrap(),
            conflicted.merged(&withdrawn, &bob.id()).unwrap(),
        ] {
            let (view, kept) = merged.view(&bob.id(), NOW + 2).unwrap();
            assert!(view.in_conflict());
            assert!(view.used().is_none());
            assert_eq!(kept.len(), 2);
            assert_eq!(merged.dropped_at, Some(NOW + 1));
        }
        // A record at exactly the floor is at or before it: dropped by a merge, and never
        // kept at a read; one past it is kept.
        let at_floor = seen(vec![signed(1, tip_jar())], None);
        let mut merged = at_floor.merged(&conflicted, &bob.id()).unwrap();
        assert_eq!(merged.records.len(), 2);
        let (_, held) = merged.view(&bob.id(), NOW + 2).unwrap();
        let again = record(&signed(1, json!({ "y": 1 })));
        assert!(merged.keep(&again, held, NOW + 2).is_none());
        assert_eq!(merged.records.len(), 2);
        assert!(!merged.past_dropped(&record(&signed(1, tip_jar())), None));
        assert!(merged.past_dropped(&record(&signed(2, tip_jar())), None));
        // A merge that drops nothing uses the record it used before.
        let older = seen(vec![signed(0, tip_jar())], None);
        let newer = seen(vec![signed(0, tip_jar()), signed(1, json!({}))], None);
        let merged = older.merged(&newer, &bob.id()).unwrap();
        assert_eq!(merged.dropped_at, None);
        let (view, _) = merged.view(&bob.id(), NOW + 1).unwrap();
        assert_eq!(view.used().unwrap().updated_at(), NOW + 1);
        // A floor far ahead of the clock holds nothing at a read, also after a merge took it:
        // a record is kept and used as usual. A restored value holding one past all it keeps
        // is refused.
        let damaged = seen(Vec::new(), Some(u64::MAX));
        assert!(!damaged.verifies(&bob.id(), &bob.app_id()));
        let mut merged = damaged.merged(&older, &bob.id()).unwrap();
        assert!(merged.records.is_empty());
        let current = record(&signed(1, tip_jar()));
        let view = merged.keep(&current, Vec::new(), NOW + 1).unwrap();
        assert_eq!(view.used().unwrap().updated_at(), NOW + 1);
        assert_eq!(merged.dropped_at, Some(u64::MAX));
        // Up to the tolerance ahead, the floor holds; past it, it holds nothing.
        let near = seen(Vec::new(), Some(NOW + 1 + CLOCK_TOLERANCE_MS));
        assert!(!near.past_dropped(&current, Some(NOW + 1)));
        assert!(near.past_dropped(&current, Some(NOW)));
        // The same floor holds again once the clock has passed it.
        assert!(!near.past_dropped(&current, Some(NOW + DAY_MS)));
        let far = seen(Vec::new(), Some(NOW + DAY_MS));
        assert!(far.past_dropped(&current, Some(NOW)));
        assert!(!far.past_dropped(&current, Some(NOW + DAY_MS)));
        let mut far = far;
        assert!(far.keep(&current, Vec::new(), NOW + DAY_MS).is_none());
        // A record kept while the floor was ahead plays no part once the clock reaches it,
        // until the next record kept drops it.
        let ahead = record(&signed(DAY_MS + 2 * CLOCK_TOLERANCE_MS, tip_jar()));
        let mut kept = seen(Vec::new(), Some(NOW + DAY_MS));
        kept.records = vec![SignedDoc::from(current.clone()), SignedDoc::from(ahead)];
        assert!(kept.verifies(&bob.id(), &bob.app_id()));
        let (view, held) = kept.view(&bob.id(), NOW + 1).unwrap();
        assert_eq!(view.used().unwrap().updated_at(), NOW + 1);
        assert_eq!(held.len(), 2);
        let (view, held) = kept.view(&bob.id(), NOW + DAY_MS).unwrap();
        assert!(view.used().is_none());
        assert_eq!(held.len(), 2);
        let next = record(&signed(DAY_MS + 1, tip_jar()));
        kept.keep(&next, held, NOW + DAY_MS).unwrap();
        assert_eq!(kept.records.len(), 2);
        assert_eq!(kept.dropped_at, Some(NOW + DAY_MS));
    }

    #[test]
    fn test_discovery_state_records_unknown_member() {
        let (_, records) = discovery_records();
        let mut value = serde_json::to_value(records).unwrap();
        value["extra"] = json!(true);
        assert!(serde_json::from_value::<SeenRecords>(value).is_err());
    }

    #[test]
    fn test_discovery_state_records_empty_is_not_persisted() {
        let (bob, _) = discovery_records();
        let records: SeenRecords =
            serde_json::from_value(json!({"records": [], "unkept_at": null, "dropped_at": null}))
                .unwrap();
        assert!(!records.verifies(&bob.id(), &bob.app_id()));
        // A group too large to keep is kept as its time alone, with a floor at or before it.
        for value in [
            json!({"records": [], "unkept_at": 5, "dropped_at": null}),
            json!({"records": [], "unkept_at": 5, "dropped_at": 5}),
        ] {
            let held: SeenRecords = serde_json::from_value(value).unwrap();
            assert!(held.verifies(&bob.id(), &bob.app_id()));
        }
        // A floor past everything kept is no value a reader writes.
        for value in [
            json!({"records": [], "unkept_at": null, "dropped_at": 5}),
            json!({"records": [], "unkept_at": 5, "dropped_at": 6}),
            json!({"records": [], "unkept_at": null, "dropped_at": u64::MAX}),
        ] {
            let held: SeenRecords = serde_json::from_value(value).unwrap();
            assert!(!held.verifies(&bob.id(), &bob.app_id()));
        }
        for value in [
            json!({"records": []}),
            json!({"records": [], "unkept_at": 5}),
            json!({"records": [], "dropped_at": 5}),
        ] {
            assert!(serde_json::from_value::<SeenRecords>(value).is_err());
        }
    }

    #[test]
    fn test_discovery_state_group_ahead_of_the_clock_holds_nothing() {
        let now = 1_000_000_000;
        let held = |at: u64| SeenRecords {
            dropped_at: None,
            records: Vec::new(),
            unkept_at: Some(at),
        };
        // A group up to the clock tolerance ahead holds what is at or before it.
        let near = held(now + CLOCK_TOLERANCE_MS);
        assert!(near.before_unkept(Some(now), now));
        assert!(near.before_unkept(None, now));
        assert!(!near.before_unkept(Some(now + CLOCK_TOLERANCE_MS + 1), now));
        // One further ahead, such as a restored value no record set, holds nothing.
        for far in [now + CLOCK_TOLERANCE_MS + 1, u64::MAX] {
            assert!(!held(far).before_unkept(Some(now), now));
            assert!(!held(far).before_unkept(None, now));
        }
    }

    #[test]
    fn test_discovery_state_records_current_structure() {
        let (bob, records) = discovery_records();
        let bytes = serde_json::to_vec(&records).unwrap();
        let read: SeenRecords = serde_json::from_slice(&bytes).unwrap();
        assert!(read.verifies(&bob.id(), &bob.app_id()));
        assert!(!read.verifies(&[9; 32], &bob.app_id()));
        assert!(!read.verifies(&bob.id(), &[9; 16]));
        assert!(SeenRecords::default()
            .view(&bob.id(), NOW)
            .unwrap()
            .0
            .used()
            .is_none());
        for value in [
            json!({}),
            json!({"records": null}),
            json!({"records": false}),
        ] {
            assert!(serde_json::from_value::<SeenRecords>(value).is_err());
        }
        let raw = String::from_utf8(bytes)
            .unwrap()
            .replacen("{", "{\"records\":[],", 1);
        assert!(serde_json::from_str::<SeenRecords>(&raw).is_err());
    }

    /// Why the record served at `app`'s path, read and verified, isn't used.
    fn why_not(
        app: &Party,
        bytes: &[u8],
        doc: &IdentityDoc,
        now: u64,
    ) -> Option<PublicAppFailureReason> {
        let record = read_record(&app.id(), &app.app_id(), bytes).unwrap();
        usable(&app.app_id(), &record, doc, device(now)).err()
    }

    #[test]
    fn test_public_read_unusable_record_named_refused_or_expired() {
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let doc = bob.doc(&[&bob]);
        let expired = |reason| matches!(reason, Some(PublicAppFailureReason::Expired(_)));
        let refused = |reason| matches!(reason, Some(PublicAppFailureReason::Refused(_)));
        let good = record_bytes(&bob, tip_jar(), NOW, Some(NOW + DAY_MS));
        assert_eq!(why_not(&bob, &good, &doc, NOW + 1), None);
        // Past its expires_at, past the record age limit, and ahead of the clock: expired.
        assert!(expired(why_not(&bob, &good, &doc, NOW + DAY_MS)));
        let far = record_bytes(&bob, tip_jar(), NOW, Some(NOW + 365 * DAY_MS));
        assert!(expired(why_not(&bob, &far, &doc, NOW + 90 * DAY_MS)));
        let ahead = record_bytes(&bob, tip_jar(), NOW + 10 * 60_000, Some(NOW + DAY_MS));
        assert!(expired(why_not(&bob, &ahead, &doc, NOW)));
        // An app not current, or a member that doesn't read: refused.
        assert!(refused(why_not(&bob, &good, &bob.doc(&[]), NOW + 1)));
        let bad = record_bytes(
            &bob,
            json!({ "public_endpoints": { "btc-lightning-bolt12": 7 } }),
            NOW,
            Some(NOW + DAY_MS),
        );
        assert!(refused(why_not(&bob, &bad, &doc, NOW + 1)));
        // A certificate without `paykit_request`: refused.
        let wallet = Party::new("bob", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bytes = record_bytes(&wallet, tip_jar(), NOW, Some(NOW + DAY_MS));
        assert!(refused(why_not(
            &wallet,
            &bytes,
            &bob.doc(&[&wallet]),
            NOW + 1
        )));
    }

    /// A member of `count` endpoints with payloads of `len` bytes.
    fn member_of(count: usize, len: usize) -> PaykitMember {
        let mut member = PaykitMember::default();
        for i in 0..count {
            member.public_endpoints.insert(
                PaymentEndpointIdentifier::new(&format!("btc-e{i}")).unwrap(),
                PaymentEndpointPayload::new(&"x".repeat(len)).unwrap(),
            );
        }
        member
    }

    #[test]
    fn test_public_read_room_takes_whole_sets_within_the_bound() {
        let mut room = Room::new();
        assert!(room.take(&member_of(MAX_PUBLIC_ENDPOINTS - 1, 1)));
        // Two don't fit in the one left, and nothing is taken of them.
        assert!(!room.take(&member_of(2, 1)));
        assert!(room.take(&member_of(1, 1)));
        assert!(!room.take(&member_of(1, 1)));
        let mut room = Room::new();
        assert!(room.take(&member_of(1, MAX_PUBLIC_PAYLOAD_BYTES - 10)));
        assert!(!room.take(&member_of(1, 11)));
        assert!(room.take(&member_of(1, 10)));
        assert!(!room.take(&member_of(1, 1)));
    }
}
