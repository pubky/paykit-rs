//! Destination checks before paying (Paykit spec 4, 6, 7.2 step 1).
//!
//! Destinations come only from the payee (Paykit spec 6): an app pays only to endpoints in a
//! verified message from the payee's current app, or to the payee's public endpoints. Before
//! money moves, the payer checks, against the payee's `identity.json` read within the last
//! 10 minutes (Switch spec 4.3):
//!
//! - the message carrying the destination is from the payee, from the app that owns the
//!   destination, and names the obligation paid by its signed fields (a request is the one
//!   paid);
//! - it carries the destination, and hasn't expired: a Private Payment List is used until the
//!   earlier of its `expires_at` and 30 days after its inbox accepted it (Paykit spec 5);
//! - the sending app's certificate allows `request` (a labelled selling scope counts), a
//!   listed certificate hasn't expired, and the app is current in the payee's
//!   `identity.json`, its certificate counting at the time the message was accepted.
//!
//! A removed app's destinations are ignored, even if its messages are still held (Paykit spec
//! 4). Public endpoints, from app records, aren't read yet ([`crate::app_records`]).
//!
//! Security: this is the check that stops a payment to a destination an attacker planted. It
//! fails closed: a payee `identity.json` that can't be read refuses the payment.

use paykit_lib::bodies::RequestDestination;
use paykit_lib::records::{DestinationSource, StoredMessage};
use paykit_lib::scopes::acts_as_payee;
use paykit_lib::Body;
use pubky_switch_client::app::DOC_CACHE_MS;
use pubky_switch_core::records::{IdentityDoc, Standing, IDENTITY_PATH};
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Result};
use crate::journal::Destination;
use crate::paykit::Paykit;
use crate::state::key;

/// A counterparty's `identity.json` as kept (`p/ident/<pubky>`), and when it was read.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct KeptIdentity {
    doc: SignedDoc,
    read_ms: u64,
}

impl Paykit {
    /// The payee's `identity.json`, read within the last 10 minutes (Switch spec 4.3): the
    /// kept copy while fresh, otherwise read from the Homeserver, keeping the higher version.
    /// `Err` names why it can't be had.
    pub(crate) async fn payee_identity(
        &mut self,
        payee: &[u8; 32],
    ) -> Result<std::result::Result<IdentityDoc, String>> {
        let now = self.now();
        let kept: Option<KeptIdentity> = self.local.get(&key::identity(payee)).await?;
        let kept_doc = kept
            .as_ref()
            .and_then(|k| IdentityDoc::verify(&k.doc, payee).ok());
        if let (Some(kept), Some(doc)) = (&kept, &kept_doc) {
            if now.saturating_sub(kept.read_ms) < DOC_CACHE_MS {
                return Ok(Ok(doc.clone()));
            }
        }
        let fetched = match self.homeserver.get(*payee, IDENTITY_PATH.to_owned()).await {
            Ok(Some(bytes)) => serde_json::from_slice::<SignedDoc>(&bytes)
                .ok()
                .and_then(|signed| IdentityDoc::verify(&signed, payee).ok()),
            Ok(None) => None,
            Err(_) => {
                return Ok(Err(
                    "Switch spec 4.3: the payee's identity.json can't be read now".into(),
                ))
            }
        };
        let Some(fetched) = fetched else {
            return Ok(Err(
                "Switch spec 4.3: the payee has no identity.json that verifies".into(),
            ));
        };
        // Readers use the higher version (Switch spec 4.3).
        let doc = match kept_doc {
            Some(kept) if kept.version() > fetched.version() => kept,
            _ => fetched,
        };
        self.local
            .put(
                &key::identity(payee),
                &KeptIdentity {
                    doc: doc.signed().clone(),
                    read_ms: now,
                },
            )
            .await?;
        Ok(Ok(doc))
    }

    /// The checks of a destination before money moves (Paykit spec 4, 7.2): `Err` names the
    /// rule that refuses it.
    pub(crate) async fn check_destination(
        &mut self,
        payee: &[u8; 32],
        destination: &Destination,
        carrier: &StoredMessage,
        request_hash: Option<&[u8; 32]>,
    ) -> Result<std::result::Result<(), String>> {
        if let Err(why) = carries(payee, destination, carrier, request_hash, self.now()) {
            return Ok(Err(why));
        }
        let doc = match self.payee_identity(payee).await? {
            Ok(doc) => doc,
            Err(why) => return Ok(Err(why)),
        };
        Ok(current(&doc, destination, carrier, self.now()))
    }
}

/// The message carries the destination, names the obligation, and hasn't expired (Paykit
/// spec 4, 5, 7.2).
pub(crate) fn carries(
    payee: &[u8; 32],
    destination: &Destination,
    carrier: &StoredMessage,
    request_hash: Option<&[u8; 32]>,
    now: u64,
) -> std::result::Result<(), String> {
    if &carrier.sender() != payee {
        return Err("Paykit spec 6: the destination comes from another identity".into());
    }
    if carrier.message().from.app_id != destination.app_id {
        return Err("Paykit wire 4.4: the destination isn't owned by the sending app".into());
    }
    let endpoint_id = destination.endpoint_id().map_err(|e| e.to_string())?;
    match (destination.source(), carrier.body()) {
        (DestinationSource::Request, Body::PaymentRequest(request)) => {
            if request_hash.is_some_and(|h| *h != carrier.hash()) {
                return Err("Paykit spec 7.2: the destination comes from another request".into());
            }
            let RequestDestination::Endpoints(endpoints) = &request.destination else {
                return Err("Paykit spec 8.1: the request carries no endpoints".into());
            };
            if endpoints
                .get(&endpoint_id)
                .is_none_or(|p| p.as_str() != destination.payload)
            {
                return Err("Paykit spec 7.2: the request doesn't carry the destination".into());
            }
            Ok(())
        }
        (DestinationSource::List, Body::PrivatePaymentList(list)) => {
            let listed = list
                .payment_endpoints
                .get(&endpoint_id)
                .is_some_and(|e| e.payload.as_str() == destination.payload);
            if !listed || destination.list_version != Some(list.version) {
                return Err("Paykit spec 7.2: the list doesn't carry the destination".into());
            }
            let accepted = carrier.accepted_at().unwrap_or(0);
            if now >= list.usable_until(accepted) {
                return Err("Paykit spec 5: the Private Payment List has expired".into());
            }
            Ok(())
        }
        _ => Err("Paykit spec 7.2: no verified message carries the destination".into()),
    }
}

/// The sending app is current in the payee's `identity.json`, with a certificate that allows
/// `request` and, when listed, hasn't expired (Paykit spec 4).
pub(crate) fn current(
    doc: &IdentityDoc,
    destination: &Destination,
    carrier: &StoredMessage,
    now: u64,
) -> std::result::Result<(), String> {
    let cert = carrier.cert();
    if !acts_as_payee(cert.scopes()) {
        return Err("Paykit spec 4: the sending app's certificate doesn't allow request".into());
    }
    let standing = doc.standing(&destination.app_id, &cert.hash());
    if standing == Standing::Listed && cert.expired_at(now) {
        return Err("Paykit spec 4: the sending app's listed certificate has expired".into());
    }
    let at = carrier.accepted_at().unwrap_or(now);
    if doc.listed_cert(&destination.app_id).is_none() || !standing.counts_at(at) {
        return Err(
            "Paykit spec 4: the sending app isn't current in the payee's identity.json".into(),
        );
    }
    Ok(())
}

/// A refusal of the destination check, as the caller sees it.
pub(crate) fn refused(why: String) -> PaykitError {
    crate::error::Refusal::Destination(why).into()
}

#[cfg(test)]
mod tests {
    use paykit_lib::{PaymentEndpointIdentifier, PaymentEndpointPayload};
    use pubky_switch_core::records::{IdentityBody, RemovedApp};
    use serde_json::json;

    use super::*;
    use crate::testkit::{object, Party, NOW};

    fn request_body() -> serde_json::Map<String, serde_json::Value> {
        object(json!({
            "amount": { "asset": "btc", "value": "1" },
            "payment_reference": "order-1",
            "proposal_expires_at": null,
            "payment_endpoints": { "btc-lightning-bolt11": "lnbc1" }
        }))
    }

    fn list_body(expires_at: u64) -> serde_json::Map<String, serde_json::Value> {
        object(json!({
            "version": 7,
            "expires_at": pubky_switch_core::time::rfc3339(expires_at),
            "payment_endpoints": { "btc-bitcoin-p2wpkh": { "payload": "bc1q", "single_use": true } }
        }))
    }

    fn destination(app: &Party, id: &str, payload: &str, source: DestinationSource) -> Destination {
        Destination::new(
            app.app_id(),
            &PaymentEndpointIdentifier::new(id).unwrap(),
            &PaymentEndpointPayload::new(payload).unwrap(),
            source,
            (source == DestinationSource::List).then_some(7),
            source == DestinationSource::List,
        )
    }

    fn parties() -> (Party, Party) {
        (
            Party::new("alice", "wallet", &["pay"]),
            Party::new("bob", "shop", &["request"]),
        )
    }

    #[test]
    fn test_destination_request_carries_its_endpoint() {
        let (alice, bob) = parties();
        let request = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let dest = destination(
            &bob,
            "btc-lightning-bolt11",
            "lnbc1",
            DestinationSource::Request,
        );
        assert_eq!(
            carries(&bob.id(), &dest, &request, Some(&request.hash()), NOW),
            Ok(())
        );
    }

    #[test]
    fn test_destination_refuses_another_identity_app_payload_or_request() {
        let (alice, bob) = parties();
        let carol = Party::new("carol", "shop", &["request"]);
        let request = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let dest = destination(
            &bob,
            "btc-lightning-bolt11",
            "lnbc1",
            DestinationSource::Request,
        );
        assert!(carries(&carol.id(), &dest, &request, None, NOW).is_err());
        let other_app = destination(
            &carol,
            "btc-lightning-bolt11",
            "lnbc1",
            DestinationSource::Request,
        );
        assert!(carries(&bob.id(), &other_app, &request, None, NOW).is_err());
        let other_payload = destination(
            &bob,
            "btc-lightning-bolt11",
            "lnbc2",
            DestinationSource::Request,
        );
        assert!(carries(&bob.id(), &other_payload, &request, None, NOW).is_err());
        assert!(carries(&bob.id(), &dest, &request, Some(&[9; 32]), NOW).is_err());
    }

    #[test]
    fn test_destination_list_used_until_it_expires() {
        let (alice, bob) = parties();
        let list = bob.message(
            &alice,
            "paykit.private_payment_list",
            vec![],
            list_body(NOW + 3_600_000),
        );
        let dest = destination(&bob, "btc-bitcoin-p2wpkh", "bc1q", DestinationSource::List);
        assert_eq!(carries(&bob.id(), &dest, &list, None, NOW), Ok(()));
        assert!(carries(&bob.id(), &dest, &list, None, NOW + 3_600_000).is_err());
        let mut other_version = dest.clone();
        other_version.list_version = Some(8);
        assert!(carries(&bob.id(), &other_version, &list, None, NOW).is_err());
    }

    #[test]
    fn test_destination_sending_app_current_with_request() {
        let (alice, bob) = parties();
        let request = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let dest = destination(
            &bob,
            "btc-lightning-bolt11",
            "lnbc1",
            DestinationSource::Request,
        );
        assert_eq!(current(&bob.doc(&[&bob]), &dest, &request, NOW), Ok(()));
        // Not listed.
        assert!(current(&bob.doc(&[]), &dest, &request, NOW).is_err());
    }

    #[test]
    fn test_destination_removed_app_and_payer_app_refused() {
        let (alice, bob) = parties();
        let request = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let dest = destination(
            &bob,
            "btc-lightning-bolt11",
            "lnbc1",
            DestinationSource::Request,
        );
        let listed = bob.doc(&[&bob]);
        let mut body: IdentityBody = listed.body().clone();
        body.version += 1;
        body.apps.clear();
        body.removed.push(RemovedApp {
            app_id: bob.app_id(),
            distrust_from: NOW - 1,
        });
        let removed = IdentityDoc::sign(&bob.key, body).unwrap();
        assert!(current(&removed, &dest, &request, NOW).is_err());
        // A destination from an app without `request` isn't one.
        let payer_app = Party::new("bob", "wallet", &["pay"]);
        let from_payer =
            payer_app.message(&alice, "paykit.payment_request", vec![], request_body());
        let doc = bob.doc(&[&payer_app]);
        let dest = destination(
            &payer_app,
            "btc-lightning-bolt11",
            "lnbc1",
            DestinationSource::Request,
        );
        assert!(current(&doc, &dest, &from_payer, NOW).is_err());
    }
}
