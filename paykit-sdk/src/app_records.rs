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
use pubky_switch_client::app::{Initiator, UserConfirmed};
use pubky_switch_core::records::{app_record_path, AppRecord, IdentityDoc};
use pubky_switch_core::sig::SignedDoc;

use crate::destination::{current, Carrier};
use crate::error::{PaykitError, Refusal, Result};
use crate::journal::Destination;
use crate::paykit::Paykit;
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

/// Whether a record read for `app_id` of `payee` may be used now (module docs). `None` when
/// it isn't: it is ignored.
pub(crate) fn usable(
    payee: &[u8; 32],
    app_id: &[u8; 16],
    bytes: &[u8],
    doc: &IdentityDoc,
    now: u64,
) -> Option<UsableRecord> {
    let signed: SignedDoc = serde_json::from_slice(bytes).ok()?;
    let record = AppRecord::verify(&signed, payee).ok()?;
    // Switch spec 4.2: a record is used only from its own app's path.
    if &record.body().app_id != app_id {
        return None;
    }
    let member = PaykitMember::read(&record).ok()?;
    if record.is_ahead(now) || record.expires_at().is_none_or(|at| at <= now) {
        return None;
    }
    let carrier = Carrier::Record(Box::new(record));
    // The app's standing, with the record's certificate, and the record age limit.
    current(doc, app_id, &carrier, now).ok()?;
    let Carrier::Record(record) = carrier else {
        return None;
    };
    let record = *record;
    Some(UsableRecord { record, member })
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
        let doc = match self.payee_identity(payee, by, confirmed).await? {
            Ok(doc) => doc,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let mut out = Vec::new();
        for listed in &doc.body().apps {
            if app.is_some_and(|a| a != listed.app_id) {
                continue;
            }
            let bytes = self
                .homeserver
                .get(*payee, app_record_path(&listed.app_id))
                .await
                .map_err(PaykitError::Homeserver)?;
            let Some(bytes) = bytes else {
                continue;
            };
            if let Some(usable) = usable(payee, &listed.app_id, &bytes, &doc, self.now()) {
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
    /// `identity.json`; [`PaykitError::Homeserver`] when a record can't be read now.
    pub async fn public_endpoints(
        &mut self,
        payee: &[u8; 32],
        confirmed: Option<UserConfirmed>,
    ) -> Result<Vec<PublicEndpoint>> {
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
        let usable = usable(&bob.id(), &bob.app_id(), &bytes, &doc, NOW + 1).unwrap();
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
        assert!(usable(&bob.id(), &bob.app_id(), &bad, &doc, NOW + 1).is_none());
        let no_expiry = record_bytes(&bob, tip_jar(), NOW, None);
        assert!(usable(&bob.id(), &bob.app_id(), &no_expiry, &doc, NOW + 1).is_none());
        // Bytes that aren't a signed record, and a record read from another app's path.
        assert!(usable(&bob.id(), &bob.app_id(), b"{}", &doc, NOW + 1).is_none());
        let good = record_bytes(&bob, tip_jar(), NOW, Some(later));
        assert!(usable(&bob.id(), &[9; 16], &good, &doc, NOW + 1).is_none());
        // Another identity's record.
        let carol = Party::new("carol", "shop", &["request"]);
        assert!(usable(&carol.id(), &bob.app_id(), &good, &doc, NOW + 1).is_none());
    }

    #[test]
    fn test_app_records_expired_stale_or_ahead_record_ignored() {
        let bob = Party::new("bob", "shop", &["request"]);
        let doc = bob.doc(&[&bob]);
        // Expired at its own expires_at.
        let bytes = record_bytes(&bob, tip_jar(), NOW, Some(NOW + DAY_MS));
        assert!(usable(&bob.id(), &bob.app_id(), &bytes, &doc, NOW + DAY_MS - 1).is_some());
        assert!(usable(&bob.id(), &bob.app_id(), &bytes, &doc, NOW + DAY_MS).is_none());
        // Past updated_at plus the identity's record age limit (90 days when absent), whatever
        // expires_at it signed.
        let far = record_bytes(&bob, tip_jar(), NOW, Some(NOW + 365 * DAY_MS));
        assert!(usable(&bob.id(), &bob.app_id(), &far, &doc, NOW + 89 * DAY_MS).is_some());
        assert!(usable(&bob.id(), &bob.app_id(), &far, &doc, NOW + 90 * DAY_MS).is_none());
        // Made more than 5 minutes ahead of the payer's clock.
        let ahead = record_bytes(&bob, tip_jar(), NOW + 10 * 60_000, Some(NOW + DAY_MS));
        assert!(usable(&bob.id(), &bob.app_id(), &ahead, &doc, NOW).is_none());
    }

    #[test]
    fn test_app_records_record_of_an_app_not_current_ignored() {
        let bob = Party::new("bob", "shop", &["request"]);
        let bytes = record_bytes(&bob, tip_jar(), NOW, Some(NOW + DAY_MS));
        assert!(usable(&bob.id(), &bob.app_id(), &bytes, &bob.doc(&[]), NOW + 1).is_none());
        // An app without `request` publishes no destination.
        let wallet = Party::new("bob", "wallet", &["pay"]);
        let bytes = record_bytes(&wallet, tip_jar(), NOW, Some(NOW + DAY_MS));
        let doc = bob.doc(&[&wallet]);
        assert!(usable(&bob.id(), &wallet.app_id(), &bytes, &doc, NOW + 1).is_none());
    }
}
