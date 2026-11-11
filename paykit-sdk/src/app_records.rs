//! App records: Paykit's member of a Switch app record, `protocols.paykit` (Paykit spec 4,
//! Paykit wire 3.2).
//!
//! **Reading** a payee's public endpoints ([`Paykit::public_endpoints`], and a payment in
//! [`crate::PaymentMode::Public`]): the payee's `identity.json` is taken as for any
//! destination (Paykit spec 7.2; Switch spec 4.3), then each current app's record is
//! read from the payee's Homeserver, at the Switch's path for it (Switch spec 4.2), and used
//! only if:
//!
//! - it verifies as the Switch's app record for the payee (Switch wire 3.2), and its
//!   `app_id` is the one of the path it was read from;
//! - the app is current in the payee's `identity.json` with the certificate the record
//!   carries, which allows `request`, and a listed certificate hasn't expired (Paykit spec
//!   4);
//! - its Paykit member reads ([`PaykitMember::read`]), a record with public endpoints
//!   carrying `expires_at` (Paykit wire 3.2);
//! - it is within its lifetime: before the earlier of its `expires_at` and its `updated_at`
//!   plus the identity's `record_max_age`, and its `updated_at` isn't ahead of the payer's
//!   clock (Paykit spec 4).
//!
//! Of one app's records, the one used is the one the Switch's comparison rules pick (Switch
//! spec 4.2, Switch wire 3.2 "Equal times"), through the core's `RecordView`: the newest
//! seen; during a conflict, two bodies at one `updated_at`, the earlier one held, if any,
//! until a strictly later record ends it. The records that took part are kept in the
//! store, so the comparison survives a restart.
//!
//! A record that fails any of these is ignored, as a reader ignores any invalid record: its
//! endpoints are never paid. A Homeserver that can't be read now is an error. The same
//! checks run again before money moves, on the record kept as the destination's carrier.
//!
//! **Publishing** this app's own Paykit member isn't done by this SDK. The Switch client
//! builds the app's record itself, at join and at renewal, with no protocol member and no
//! `expires_at`; it has no call to add `protocols.paykit`, set `expires_at`, sign the record
//! again and deliver it with `app.put`, or renew it once half its lifetime has passed
//! (Paykit wire 3.2, "Renewal"); and it writes nothing to the Homeserver. Until it does,
//! apps keep `AppConfig::publishes_record` off.
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
pub struct PublicEndpoint {
    /// The payee app that published it.
    pub app_id: [u8; 16],
    /// The Payment Endpoint Identifier.
    pub endpoint_id: PaymentEndpointIdentifier,
    /// The payload, which only the rail interprets.
    pub payload: PaymentEndpointPayload,
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
/// `None` when it isn't: it is ignored.
pub(crate) fn usable(
    app_id: &[u8; 16],
    record: &AppRecord,
    doc: &IdentityDoc,
    now: u64,
) -> Option<UsableRecord> {
    let member = PaykitMember::read(record).ok()?;
    if record.is_ahead(now) || record.expires_at().is_none_or(|at| at <= now) {
        return None;
    }
    let carrier = Carrier::Record(Box::new(record.clone()));
    // The app's standing, with the record's certificate, and the record age limit.
    current(doc, app_id, &carrier, now).ok()?;
    Some(UsableRecord {
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

/// The records of one payee app a reader has seen that took part in its comparison (Switch
/// spec 4.2, Switch wire 3.2 "Equal times"), oldest first (`p/ar/<payee>/<app>`). Replayed
/// in order, they rebuild the core's [`RecordView`]; kept, they survive a restart, so a
/// copy older than one seen, or one conflicting with it, is never used after a reload.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct SeenRecords {
    records: Vec<SignedDoc>,
}

impl SeenRecords {
    /// Keep `record`, which took part, after `held`, the records kept, verified in order.
    /// Past [`SEEN_RECORDS`] the oldest are dropped, but never one at the newest `updated_at`:
    /// those are the newest record and, during a conflict, every side of it, so a conflict
    /// is never forgotten and neither side becomes usable by compaction (Switch spec 4.2).
    /// Dropping an older record can only lose the earlier record a conflict falls back to,
    /// which leaves no record used rather than a wrong one.
    fn keep(&mut self, record: &AppRecord, held: &[AppRecord]) {
        self.records.push(record.signed().clone());
        let times: Vec<u64> = held
            .iter()
            .map(AppRecord::updated_at)
            .chain(std::iter::once(record.updated_at()))
            .collect();
        let newest = times.iter().copied().max().unwrap_or(0);
        let excess = self.records.len().saturating_sub(SEEN_RECORDS);
        let droppable = times.iter().take_while(|t| **t < newest).count();
        self.records.drain(..excess.min(droppable));
    }

    /// Whether every kept record is `payee`'s, verified as its signer signed it, and of its
    /// app `app_id`: what [`SeenRecords::view`] requires, checked before a restore installs
    /// a copy (`p/ar/<payee>/<app>`), so none can fail every later read of the payee.
    pub(crate) fn verifies(&self, payee: &[u8; 32], app_id: &[u8; 16]) -> bool {
        self.records.iter().all(|signed| {
            AppRecord::verify(signed, payee).is_ok_and(|record| record.cert().app_id() == *app_id)
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
/// one more body at that `updated_at` keeps the conflict as it is.
fn takes_part(record: &AppRecord, held: &[AppRecord], view: &RecordView, now: u64) -> bool {
    let adds_nothing = |h: &AppRecord| match record.order(h) {
        RecordOrder::Same | RecordOrder::Earlier => true,
        RecordOrder::Conflict => view.in_conflict(),
        RecordOrder::Later => false,
    };
    !record.is_ahead(now) && !held.iter().any(adds_nothing)
}

impl Paykit {
    /// The payee's app records that may be used now, with the `identity.json` they were
    /// checked against (module docs), of `app` only when given. `Err` inside holds the
    /// refusal of the payee's `identity.json`.
    pub(crate) async fn public_records(
        &mut self,
        payee: &[u8; 32],
        app: Option<[u8; 16]>,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
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
        let (doc, reads) = self
            .payee_identity_with(payee, Need::SwitchCopy, by, confirmed, start)
            .await?;
        let doc = match doc {
            Ok(identity) => identity.doc,
            Err(refusal) => return Ok(Err(refusal)),
        };
        // The reads start with the first copy taken, so they ran.
        let Some(mut reads) = reads else {
            return Ok(Ok(Vec::new()));
        };
        // A newer copy the switch gave may list apps the first didn't: their records are
        // read now.
        for listed in &doc.body().apps {
            if app.is_some_and(|a| a != listed.app_id)
                || reads.iter().any(|(id, _)| *id == listed.app_id)
            {
                continue;
            }
            let bytes = self
                .homeserver
                .get(*payee, app_record_path(&listed.app_id))
                .await;
            reads.push((listed.app_id, bytes));
        }
        let mut out = Vec::new();
        for (app_id, bytes) in reads {
            // Only the apps the copy acted on lists.
            let Some(listed) = doc.body().apps.iter().find(|a| a.app_id == app_id) else {
                continue;
            };
            let bytes = bytes.map_err(PaykitError::Homeserver)?;
            let read = bytes.and_then(|b| read_record(payee, &listed.app_id, &b));
            let now = self.now();
            // The record served is compared with those seen before (Switch spec 4.2): an older
            // one never replaces a newer one, and two bodies at one `updated_at` are a
            // conflict, during which neither is used and the reader goes on with the earlier
            // record it holds, until a strictly later one ends it.
            let seen_key = key::seen_records(payee, &listed.app_id);
            let mut seen = self
                .local_get::<SeenRecords>(&seen_key)
                .await?
                .unwrap_or_default();
            let (mut view, held) = seen.view(payee, now)?;
            // Only a record whose Paykit member reads takes part (Paykit wire 3.2): a reader
            // refuses a malformed one as it refuses any invalid record, and it never displaces
            // a valid one.
            let read = read.filter(|record| PaykitMember::read(record).is_ok());
            if let Some(record) = read {
                if takes_part(&record, &held, &view, now) {
                    view.see(&record, now);
                    seen.keep(&record, &held);
                    self.local_put(&seen_key, &seen).await?;
                }
            }
            let Some(used) = view.used() else {
                continue;
            };
            if let Some(usable) = usable(&listed.app_id, used, &doc, now) {
                out.push(usable);
            }
        }
        Ok(Ok(out))
    }

    /// The public endpoints of `payee`'s current apps that may be paid now (Paykit spec 4):
    /// read from their records on the payee's Homeserver and checked as the module docs say.
    /// `confirmed` is the user's confirmation for a payee whose switch couldn't confirm its
    /// apps ([`Refusal::IdentityUnconfirmed`]).
    ///
    /// # Errors
    ///
    /// [`Refusal::IdentityUnavailable`] or [`Refusal::IdentityUnconfirmed`] for the payee's
    /// `identity.json`; [`PaykitError::Homeserver`] when a record can't be read now;
    /// [`Refusal::CertificateRefused`] under a certificate Paykit refuses.
    pub async fn public_endpoints(
        &mut self,
        payee: &[u8; 32],
        confirmed: Option<UserConfirmed>,
    ) -> Result<Vec<PublicEndpoint>> {
        self.refresh_own()?;
        let records = self
            .public_records(payee, None, Initiator::User, confirmed)
            .await??;
        Ok(records
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
            .collect())
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
        usable(app_id, &read_record(payee, app_id, bytes)?, doc, now)
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
        let bob = Party::new("bob", "shop", &["request"]);
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
        let bob = Party::new("bob", "shop", &["request"]);
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
        let carol = Party::new("carol", "shop", &["request"]);
        assert!(usable_bytes(&carol.id(), &bob.app_id(), &good, &doc, NOW + 1).is_none());
    }

    #[test]
    fn test_app_records_expired_stale_or_ahead_record_ignored() {
        let bob = Party::new("bob", "shop", &["request"]);
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
        // 3.1): `request` needs exactly `conversation.add`.
        let missing = Party::with_permissions("bob", "shop", &["request"], &[]);
        let extra = Party::with_permissions(
            "bob",
            "shop",
            &["request"],
            &[Permission::ListsRead, Permission::ConversationAdd],
        );
        for app in [&missing, &extra] {
            let doc = app.doc(&[app]);
            let bytes = record_bytes(app, tip_jar(), NOW, Some(NOW + DAY_MS));
            assert!(usable_bytes(&app.id(), &app.app_id(), &bytes, &doc, NOW + 1).is_none());
        }
    }

    #[test]
    fn test_app_records_record_of_an_app_not_current_ignored() {
        let bob = Party::new("bob", "shop", &["request"]);
        let bytes = record_bytes(&bob, tip_jar(), NOW, Some(NOW + DAY_MS));
        assert!(usable_bytes(&bob.id(), &bob.app_id(), &bytes, &bob.doc(&[]), NOW + 1).is_none());
        // An app without `request` publishes no destination.
        let wallet = Party::new("bob", "wallet", &["pay"]);
        let bytes = record_bytes(&wallet, tip_jar(), NOW, Some(NOW + DAY_MS));
        let doc = bob.doc(&[&wallet]);
        assert!(usable_bytes(&bob.id(), &wallet.app_id(), &bytes, &doc, NOW + 1).is_none());
    }
}
