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
//! - its Paykit member reads ([`PaykitMember::read`]), a record with public endpoints
//!   carrying `expires_at` (Paykit wire 3.2);
//! - it is within its lifetime: before the earlier of its `expires_at` and its `updated_at`
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
use std::sync::Arc;

use pubky_switch_client::app::{Initiator, UserConfirmed};
use pubky_switch_client::error::TransportError;
use pubky_switch_client::transport::BoxFuture;
use pubky_switch_core::enc::zbase32_encode;
use pubky_switch_core::records::{
    app_record_path, AppRecord, IdentityDoc, RecordOrder, RecordView,
};
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};

use crate::destination::{current, Carrier, Need};
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
        let endpoints = member.public_endpoints.len();
        let payload_bytes = member
            .public_endpoints
            .values()
            .map(|payload| payload.as_str().len())
            .sum::<usize>();
        if endpoints > self.endpoints || payload_bytes > self.payload_bytes {
            return false;
        }
        self.endpoints -= endpoints;
        self.payload_bytes -= payload_bytes;
        true
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
    now: u64,
) -> std::result::Result<UsableRecord, PublicAppFailureReason> {
    let member = PaykitMember::read(record).map_err(|error| {
        PublicAppFailureReason::Refused(format!(
            "Paykit wire 3.2: the record's Paykit member doesn't read: {error}"
        ))
    })?;
    let expired = |limit: &str| PublicAppFailureReason::Expired(format!("Paykit spec 4: {limit}"));
    if record.is_ahead(now) {
        return Err(expired("the record was made ahead of the payer's clock"));
    }
    match record.expires_at() {
        None => {
            return Err(PublicAppFailureReason::Refused(
                "Paykit wire 3.2: the record carries no expires_at".into(),
            ))
        }
        Some(at) if at <= now => return Err(expired("the record is past its expires_at")),
        Some(_) => {}
    }
    if now >= record.trusted_until(doc.record_max_age_ms() / 1000) {
        return Err(expired(
            "the record is past the identity's record age limit",
        ));
    }
    let carrier = Carrier::Record(Box::new(record.clone()));
    // The app's standing, with the record's certificate (the record age limit passed above).
    current(doc, app_id, &carrier, now).map_err(PublicAppFailureReason::Refused)?;
    Ok(UsableRecord {
        record: record.clone(),
        member,
    })
}

/// The Homeserver's answers for each app's record path.
type RecordReads = Vec<(
    [u8; 16],
    std::result::Result<Option<Vec<u8>>, TransportError>,
)>;

/// How many records of one payee app are kept for its comparison state. Each kept record is
/// strictly later than the one before it, or conflicts with it once: at a renewal a fortnight,
/// that is years of records. Past it the oldest is dropped, which can only lose an earlier
/// record a long-lasting conflict would fall back to; the core's `RecordView` can't be kept
/// in a store as it is, so it is rebuilt from these.
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
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SeenRecords {
    records: Vec<SignedDoc>,
}

impl SeenRecords {
    /// Keep `record`, which took part, after `held`, the records kept, verified in order.
    /// Past [`SEEN_RECORDS`] the oldest are dropped, but never one at the newest `updated_at`:
    /// those are the newest record and, during a conflict, every side of it, so a conflict
    /// is never forgotten and neither side becomes usable by compaction (Switch spec 4.2).
    /// Dropping an older record can only lose the earlier record a conflict falls back to,
    /// which leaves no record used rather than a wrong one. Past [`SEEN_RECORDS_BYTES`] the
    /// oldest are dropped the same way; `false` when the records at the newest `updated_at`
    /// alone are over it, and so can't be kept. Records go by whole `updated_at`: both sides
    /// of an older conflict go together, never one alone.
    fn keep(&mut self, record: &AppRecord, held: &[AppRecord]) -> bool {
        self.records.push(record.signed().clone());
        let times: Vec<u64> = held
            .iter()
            .map(AppRecord::updated_at)
            .chain(std::iter::once(record.updated_at()))
            .collect();
        let newest = times.iter().copied().max().unwrap_or(0);
        let droppable = times.iter().take_while(|t| **t < newest).count();
        // Each record's encoded length, with its separator in the list.
        let sizes: Vec<usize> = self
            .records
            .iter()
            .map(|signed| serde_json::to_vec(signed).map_or(usize::MAX, |b| b.len() + 1))
            .collect();
        let mut bytes = sizes.iter().fold(0usize, |sum, n| sum.saturating_add(*n));
        let mut drop = self.records.len().saturating_sub(SEEN_RECORDS);
        for size in &sizes[..drop.min(droppable)] {
            bytes = bytes.saturating_sub(*size);
        }
        while drop < droppable && bytes > SEEN_RECORDS_BYTES {
            bytes = bytes.saturating_sub(sizes[drop]);
            drop += 1;
        }
        // A whole `updated_at` goes or none of it: one side of an older conflict, kept
        // without the other, would replay as a record no conflict touched and could become
        // the earlier record a later conflict falls back to (Switch spec 4.2). The times
        // dropped are all older than the newest, which is never dropped.
        let gone: Vec<u64> = times[..drop.min(droppable)].to_vec();
        let mut at = 0;
        self.records.retain(|_| {
            let keep = !gone.contains(&times[at]);
            at += 1;
            keep
        });
        let bytes = self
            .records
            .iter()
            .map(|signed| serde_json::to_vec(signed).map_or(usize::MAX, |b| b.len() + 1))
            .fold(0usize, usize::saturating_add);
        bytes <= SEEN_RECORDS_BYTES
    }

    /// Whether the kept records are nonempty and each is `payee`'s, verified as its signer
    /// signed it, and of app `app_id`: checked before a restore installs
    /// a copy (`p/ar/<payee>/<app>`), so none can fail every later read of the payee.
    pub(crate) fn verifies(&self, payee: &[u8; 32], app_id: &[u8; 16]) -> bool {
        !self.records.is_empty()
            && self.records.iter().all(|signed| {
                AppRecord::verify(signed, payee)
                    .is_ok_and(|record| record.cert().app_id() == *app_id)
            })
    }

    /// The view the records give, at the reader's clock `now`. A kept record that no longer
    /// verifies is a corrupt store.
    fn view(&self, payee: &[u8; 32], now: u64) -> Result<(RecordView, Vec<AppRecord>)> {
        let mut view = RecordView::new();
        let mut records = Vec::with_capacity(self.records.len());
        for signed in &self.records {
            let record = AppRecord::verify(signed, payee).map_err(|_| PaykitError::corrupt())?;
            view.see(&record, now);
            records.push(record);
        }
        Ok((view, records))
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
    ) -> Result<std::result::Result<Vec<UsableRecord>, Refusal>> {
        // The current apps' records are read from the Homeserver beside the payee's switch
        // lookup, from the copy of identity.json taken first (Paykit spec 4, 14).
        let homeserver = Arc::clone(&self.homeserver);
        let owner = *payee;
        let start = move |doc: &IdentityDoc| -> Option<BoxFuture<'static, RecordReads>> {
            let apps: Vec<[u8; 16]> = doc
                .body()
                .apps
                .iter()
                .map(|a| a.app_id)
                .filter(|id| app.is_none_or(|a| a == *id))
                .collect();
            Some(Box::pin(async move {
                let mut out = Vec::with_capacity(apps.len());
                for app_id in apps {
                    out.push((
                        app_id,
                        homeserver.get(owner, app_record_path(&app_id)).await,
                    ));
                }
                out
            }))
        };
        let (identity, reads) = self
            .payee_identity_with(payee, Need::SwitchCopy, by, confirmed, start)
            .await?;
        let identity = match identity {
            Ok(identity) => identity,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let fresh_until = identity.fresh_until_ms();
        let mut doc = identity.doc;
        // The reads start with the first copy taken, so they ran.
        let Some(mut reads) = reads else {
            return Ok(Ok(Vec::new()));
        };
        // Only the apps the copy acted on lists, in its order (Paykit spec 4). A newer copy
        // the switch gave may list apps the first didn't: their records are read now, all of
        // them before any app is judged.
        let mut fetched = Vec::new();
        for listed in &doc.body().apps {
            let app_id = listed.app_id;
            if app.is_some_and(|a| a != app_id) {
                continue;
            }
            let bytes = match reads.iter().position(|(id, _)| *id == app_id) {
                Some(at) => reads.swap_remove(at).1,
                None => self.homeserver.get(*payee, app_record_path(&app_id)).await,
            };
            fetched.push((app_id, bytes));
        }
        // One time judges every app of the read, taken once every record is in: no app
        // expires while another's record is read, and the read's bound goes, in the order of
        // identity.json, to the apps usable at that time. A copy of identity.json that went
        // stale while the records were read is looked up again at that time, as any copy
        // over 10 minutes old is before it is acted on (Switch spec 4.3), and only the apps
        // the fresh copy lists are judged, in its order: an app it no longer lists offers
        // nothing, and one it newly lists is read now.
        let mut now = self.now();
        if now >= fresh_until {
            doc = match self
                .payee_identity(payee, Need::SwitchCopy, by, confirmed)
                .await?
            {
                Ok(identity) => identity.doc,
                Err(refusal) => return Ok(Err(refusal)),
            };
            let mut reconciled = Vec::with_capacity(fetched.len());
            for listed in &doc.body().apps {
                let app_id = listed.app_id;
                if app.is_some_and(|a| a != app_id) {
                    continue;
                }
                let bytes = match fetched.iter().position(|(id, _)| *id == app_id) {
                    Some(at) => fetched.swap_remove(at).1,
                    None => self.homeserver.get(*payee, app_record_path(&app_id)).await,
                };
                reconciled.push((app_id, bytes));
            }
            fetched = reconciled;
            now = self.now();
        }
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
            let served_refused = read.is_none();
            if let Some(record) = read {
                if takes_part(&record, &held, &view, now) {
                    // A record the reader can't keep for the comparison is never used, nor
                    // is an earlier one in its place, which it may withdraw: the app's
                    // failure, and the other apps' reads go on (Paykit spec 4, below).
                    if !seen.keep(&record, &held) {
                        failures.push(fail(PublicAppFailureReason::Refused(format!(
                            "Paykit spec 4: the app's newest records are over the \
                             {SEEN_RECORDS_BYTES} bytes a reader keeps of them"
                        ))));
                        continue;
                    }
                    view.see(&record, now);
                    self.local_put(&seen_key, &seen).await?;
                }
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
            match usable(&app_id, used, &doc, now) {
                // An app's endpoints are taken whole or not at all (Paykit spec 4).
                Ok(usable) if room.take(&usable.member) => usable_records.push(usable),
                Ok(_) => failures.push(fail(PublicAppFailureReason::OverBound)),
                Err(reason) => failures.push(fail(reason)),
            }
        }
        Ok(Ok(usable_records))
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
    /// `identity.json`; [`Refusal::CertificateRefused`] under a certificate Paykit refuses.
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
        let records = self
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

    /// A record served at `app_id`'s path, read and checked as one used.
    fn usable_bytes(
        payee: &[u8; 32],
        app_id: &[u8; 16],
        bytes: &[u8],
        doc: &IdentityDoc,
        now: u64,
    ) -> Option<UsableRecord> {
        usable(app_id, &read_record(payee, app_id, bytes)?, doc, now).ok()
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
        let bytes = record_bytes(&bob, tip_jar(), NOW, Some(NOW + 30 * DAY_MS));
        let usable = usable_bytes(&bob.id(), &bob.app_id(), &bytes, &doc, NOW + 1).unwrap();
        let (id, payload) = usable.member.public_endpoints.iter().next().unwrap();
        let destination = usable.destination(id, payload);
        let carrier = Carrier::Record(Box::new(usable.record));
        assert_eq!(
            carries(&bob.id(), &destination, &carrier, None, NOW + 1),
            Ok(())
        );
        // Another payload isn't carried.
        let mut other = destination.clone();
        other.payload = "lno1other".into();
        assert!(carries(&bob.id(), &other, &carrier, None, NOW + 1).is_err());
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
        // A payload that isn't a string, and public endpoints without expires_at (Paykit wire
        // 3.2).
        let bad = record_bytes(
            &bob,
            json!({ "public_endpoints": { "btc-lightning-bolt12": 7 } }),
            NOW,
            Some(later),
        );
        assert!(usable_bytes(&bob.id(), &bob.app_id(), &bad, &doc, NOW + 1).is_none());
        let no_expiry = record_bytes(&bob, tip_jar(), NOW, None);
        assert!(usable_bytes(&bob.id(), &bob.app_id(), &no_expiry, &doc, NOW + 1).is_none());
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
        records.keep(&record, &[]);
        (bob, records)
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
        let records: SeenRecords = serde_json::from_value(json!({"records": []})).unwrap();
        assert!(!records.verifies(&bob.id(), &bob.app_id()));
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
        usable(&app.app_id(), &record, doc, now).err()
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
