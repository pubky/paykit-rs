//! The app's own record: Paykit's member `protocols.paykit`, with the app's public Payment
//! Endpoints and the Payment Endpoint Identifiers its payment method pays (Paykit spec 4,
//! Paykit wire 3.2).
//!
//! After a restore, public mutations wait for the identity-confirmed first read, and
//! scheduled upkeep runs only after a completed read (Switch spec 12.4).
//!
//! **Publishing.** Each public endpoint is one entry of the record's `public_endpoints`, by
//! its Payment Endpoint Identifier; [`Paykit::set_public_endpoint`],
//! [`Paykit::remove_public_endpoint`] and [`Paykit::set_public_endpoints`] change them, and
//! [`Paykit::own_public_endpoints`] lists the ones the app's record holds. Once a change can be signed,
//! the Switch client keeps the new record, delivers it to the own switch with
//! `app.put` and then writes to the own Homeserver, in that order and in the same bytes
//! (Switch spec 4.2, [`pubky_switch_client::app::App::set_protocol_member_with_expiry`]). A delivery
//! or a Homeserver write that fails stays owed by the client and goes again at the next
//! read; until then readers of the Homeserver see the record published before. The client's
//! signed record shows the last signed endpoints. The client can also retain an unsigned
//! member edit when a sibling protocol blocks signing. Paykit keeps no separate copy, and
//! nothing is issued, reserved or released for them, since a public endpoint is reusable.
//!
//! Only an app holding `paykit_request` itself, configured to publish its record
//! ([`pubky_switch_client::app::AppConfig::publishes_record`]), publishes public endpoints:
//! a payment to one carries no label, so its proof reaches only the payee's apps holding
//! `paykit_request` (Paykit spec 4, 8.4). An app that loses `paykit_request`, or stops
//! publishing, holds no public endpoints: every record it signs from then on carries none,
//! whichever path signs it (a removal, the upkeep at a receive or an update, or the join),
//! so a removal there withdraws them all. With publishing disabled, the client writes
//! nothing to the Homeserver: withdraw endpoints before disabling it to update that copy. Stale copies remain payable until
//! expiry or until a payer sees the app revoked (Paykit spec 4).
//!
//! **Expiry and renewal.** The default public offer has no Paykit deadline. A host can
//! use [`Paykit::set_public_endpoints`] to set a fixed Unix-millisecond cutoff
//! for all its public endpoints. The signed app record uses the earliest deadline
//! requested by any protocol. Readers also enforce the signer-selected record age
//! limit (90 days by default). Upkeep past half this effective lifetime refreshes
//! the timestamp and preserves a fixed cutoff. Once the cutoff is reached, upkeep
//! withdraws the endpoints. A sibling protocol may still require the record.
//!
//! **Capabilities.** `capabilities.endpoint_identifiers` lists what this app's payment
//! method pays ([`crate::Rail::endpoint_identifiers`]) at an app holding `paykit_pay`, and
//! nothing at one without (Paykit wire 3.2). At the join and at every receive the record is
//! brought to that list when it differs, and
//! [`Paykit::update_own_record`] does so at once. A list the record can't carry, one that
//! would make its `app.put` larger than the Switch's cap on a signed operation (Switch wire
//! 6.1), is refused the same way wherever it is read: at the join before anything is
//! written, and at a receive, an update or a publication before anything is signed.
//!
//! Security: the record is public by design, readable by anyone on the Homeserver (Paykit
//! spec 4, "What is public"); it goes to no counterparty, so no deny list gates it. Every
//! identifier and payload it carries went through the validated constructors
//! ([`PaymentEndpointIdentifier`], [`PaymentEndpointPayload`]); a payer reading it still
//! checks the signature, the app's standing and the record's lifetime before paying.

use std::collections::BTreeMap;

use paykit_lib::app_record::{PaykitMember, PROTOCOL_NAME};
use paykit_lib::{PaymentEndpointIdentifier, PaymentEndpointPayload};
use pubky_switch_client::app::{AppError, ProtocolRecord, Publication, RecordExpiry};
use pubky_switch_client::error::StoreError;
use pubky_switch_core::ops::{sign_batch, OpKind};
use pubky_switch_core::records::{AppCert, AppRecord, AppRecordBody};
use pubky_switch_core::sig::SignedDoc;
use pubky_switch_core::signing::Signs;

use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::rail::Rail;

/// The most public endpoints the app publishes: a payer's read takes at most this many over
/// all of a payee's apps, so a record with more would never be paid (Paykit spec 4).
pub const MAX_OWN_PUBLIC_ENDPOINTS: usize = crate::app_records::MAX_PUBLIC_ENDPOINTS;

impl Paykit {
    /// The public endpoints the app's record holds now, by Payment Endpoint Identifier: the
    /// latest record the app signed, which the switch may not have accepted yet.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] after a client call that can change the own record ended with
    /// the store unable to tell whether it was applied, until the app is loaded again
    /// ([`Paykit::load`]): the record this handle holds may not be the one stored, and could
    /// show an endpoint already withdrawn. A corrupt store when the record held doesn't
    /// verify or its Paykit member doesn't read.
    pub fn own_public_endpoints(
        &self,
    ) -> Result<BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>> {
        self.own_record_known()?;
        Ok(self.own_member()?.public_endpoints)
    }

    /// Publish `payload` as the app's public endpoint `endpoint`, replacing the one it held
    /// there (module docs). The other public endpoints stay.
    ///
    /// # Errors
    ///
    /// As [`Paykit::set_public_endpoints`].
    pub async fn set_public_endpoint(
        &mut self,
        endpoint: PaymentEndpointIdentifier,
        payload: PaymentEndpointPayload,
    ) -> Result<Publication> {
        Box::pin(self.edit_public_endpoint(endpoint, Some(payload))).await
    }

    /// Stop publishing the app's public endpoint `endpoint`; one the record doesn't hold is
    /// removed already. A record is still signed when something else of it is owed: the
    /// capabilities the payment method pays changed, or a restored record the switch never
    /// accepted is signed anew ([`Paykit::set_public_endpoints`]).
    ///
    /// # Errors
    ///
    /// As [`Paykit::set_public_endpoints`].
    pub async fn remove_public_endpoint(
        &mut self,
        endpoint: &PaymentEndpointIdentifier,
    ) -> Result<Publication> {
        Box::pin(self.edit_public_endpoint(endpoint.clone(), None)).await
    }

    async fn edit_public_endpoint(
        &mut self,
        endpoint: PaymentEndpointIdentifier,
        payload: Option<PaymentEndpointPayload>,
    ) -> Result<Publication> {
        let desired = Box::pin(self.publication_for_edit()).await?;
        let member = PaykitMember::from_member(desired.member.as_ref())?;
        let (mut endpoints, expiry) = if Self::cutoff_reached(desired.expiry, self.record_time()) {
            (BTreeMap::new(), RecordExpiry::Never)
        } else {
            (member.public_endpoints, desired.expiry)
        };
        if let Some(payload) = payload {
            endpoints.insert(endpoint, payload);
        } else {
            endpoints.remove(&endpoint);
        }
        Box::pin(self.offer_endpoints(endpoints, desired, expiry)).await
    }

    /// Offer exactly these public endpoints with an optional fixed cutoff in Unix
    /// milliseconds. An empty map withdraws the offer and clears Paykit's deadline.
    /// Renewal, single-endpoint edits and reopen preserve an existing offer's cutoff.
    /// Other protocols can ask for an earlier deadline for the signed record.
    ///
    /// # Errors
    ///
    /// An unavailable first read after restore, a missing selling grant or publication
    /// setting, too many endpoints, a cutoff outside the writable time range, or a
    /// client error. A valid edit waiting for a sibling to renew its record is
    /// reported as [`PaykitError::PublicEndpointChangePending`].
    pub async fn set_public_endpoints(
        &mut self,
        endpoints: BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
        expires_at_ms: Option<u64>,
    ) -> Result<Publication> {
        let desired = Box::pin(self.publication_for_edit()).await?;
        let expiry = expires_at_ms.map_or(RecordExpiry::Never, RecordExpiry::At);
        Box::pin(self.offer_endpoints(endpoints, desired, expiry)).await
    }

    async fn publication_for_edit(&mut self) -> Result<ProtocolRecord> {
        self.own_record_known()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.desired_record().await
    }

    async fn desired_record(&mut self) -> Result<ProtocolRecord> {
        let read = self.app.protocol_record(PROTOCOL_NAME).await;
        self.note_own_record_write(&read);
        Ok(read?)
    }

    async fn offer_endpoints(
        &mut self,
        endpoints: BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
        desired: ProtocolRecord,
        expiry: RecordExpiry,
    ) -> Result<Publication> {
        // Fewer endpoints, none included, are always the app's to publish: an app that lost
        // `paykit_request` or stopped publishing can still clear its held endpoints.
        let held = PaykitMember::from_member(desired.member.as_ref())?;
        let removes_only = endpoints
            .iter()
            .all(|(id, payload)| held.public_endpoints.get(id) == Some(payload));
        if !self.holds("paykit_request") && !removes_only {
            return Err(Refusal::ScopeMissing("paykit_request".into()).into());
        }
        if !removes_only && !self.app.publishes_record() {
            return Err(Refusal::Usage(
                "Paykit spec 4: an app that doesn't publish its record has no public endpoints"
                    .into(),
            )
            .into());
        }
        if endpoints.len() > MAX_OWN_PUBLIC_ENDPOINTS {
            return Err(Refusal::Usage(format!(
                "Paykit spec 4: a payer reads at most {MAX_OWN_PUBLIC_ENDPOINTS} public endpoints"
            ))
            .into());
        }
        let member = self.own_member_with(endpoints);
        Box::pin(self.put_member(member, desired, expiry)).await
    }

    /// Bring the app's record up to date now (module docs), as each receive and the join do:
    /// for a host whose payment method changed what it pays, so the record lists it at once.
    /// When the record held is already up to date, what is owed of it goes instead: its
    /// delivery to the own switch, as after one that failed once the record was kept, and its
    /// Homeserver write ([`pubky_switch_client::app::App::publish_record`]).
    ///
    /// Returns where the record stands on the Homeserver after the call, as
    /// [`Paykit::set_public_endpoints`] does: [`Publication::Owed`] when the Homeserver write
    /// failed, which the client retries at the next read or update, or
    /// [`Publication::Unchecked`] when the read that checks the Homeserver copy failed.
    /// [`Publication::AwaitingRecord`] means the client retains a newer record choice
    /// and is waiting for it to settle before repairing the public copy.
    ///
    /// # Errors
    ///
    /// [`Refusal::CertificateRefused`]; [`Refusal::IdentityUnavailable`] while a restore's
    /// first read cannot confirm the own identity; the client's refusal, store failure,
    /// alarm, first-read or delivery error; [`Refusal::NotNow`] after an unknown store
    /// result, until the app is loaded again, as [`Paykit::set_public_endpoints`] says. A
    /// deferred endpoint withdrawal is reported as [`PaykitError::PublicEndpointChangePending`].
    /// A capabilities list the record can't carry leaves the signed record unchanged
    /// (module docs). A delivery that fails stays owed by the client and goes again at the
    /// next read or update.
    pub async fn update_own_record(&mut self) -> Result<Publication> {
        self.own_record_known()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        match Box::pin(self.tend_own_record()).await? {
            Some(publication) => Ok(publication),
            None => {
                let published = Box::pin(self.app.publish_record()).await;
                self.note_own_record_write(&published);
                Ok(published?)
            }
        }
    }

    /// Bring the app's record up to date at a receive (module docs): its capabilities to
    /// what the payment method pays, and a record with public endpoints renewed once less
    /// than half of its lifetime is left. Returns where a record changed or renewed stands on
    /// the Homeserver, and `None` when the record held went to the client unchanged. A
    /// delivery that fails stays owed by the client, which sends it again at a later read; a
    /// failure before the client kept the record leaves the record
    /// held unchanged, so the next call finds it differs and puts it again.
    ///
    /// # Errors
    ///
    /// The client's refusal, store failure, alarm or delivery error; the refusal of a
    /// capabilities list the resolved record cannot carry (Switch wire 6.1).
    pub(crate) async fn tend_own_record(&mut self) -> Result<Option<Publication>> {
        // Nothing is signed while the record this handle holds may not be the one stored:
        // the next load reads it again (`own_record_known`).
        if self.own_record_unknown {
            return Ok(None);
        }
        let desired = self.desired_record().await?;
        let held = PaykitMember::from_member(desired.member.as_ref())?;
        // The cutoff and half-life checks share one observation, even if the host clock
        // rolls back before the client signs the renewed record.
        let now = self.record_time();
        let endpoints = if Self::cutoff_reached(desired.expiry, now) {
            BTreeMap::new()
        } else {
            held.public_endpoints.clone()
        };
        let wanted = self.own_member_with(endpoints);
        let expiry = if wanted.public_endpoints.is_empty() {
            RecordExpiry::Never
        } else {
            desired.expiry
        };
        if desired.pending || wanted != held || expiry != desired.expiry {
            return Box::pin(self.put_member(wanted, desired, expiry))
                .await
                .map(Some);
        }
        if !held.public_endpoints.is_empty() && self.renewal_due(now)? {
            let renewed = match expiry {
                RecordExpiry::Never => self.app.renew_record(PROTOCOL_NAME, None).await,
                RecordExpiry::At(at) => self.app.renew_record(PROTOCOL_NAME, Some(at)).await,
                RecordExpiry::Lifetime(ms) => {
                    self.app
                        .renew_record_with_lifetime(PROTOCOL_NAME, Some(ms))
                        .await
                }
                _ => {
                    return Err(
                        Refusal::NotNow("unknown Switch app-record expiry policy".into()).into(),
                    );
                }
            };
            self.note_own_record_write(&renewed);
            return Ok(Some(renewed?));
        }
        // The unchanged-member call retains the exact policy and optional fields. A
        // restored record never accepted by the service still gets its required new delivery.
        let result = self
            .app
            .set_protocol_member_with_lifetime(PROTOCOL_NAME, desired.member, None)
            .await;
        self.note_own_record_write(&result);
        result?;
        Ok(None)
    }

    fn record_time(&self) -> u64 {
        self.now().max(self.app.highest_service_time().unwrap_or(0))
    }

    fn cutoff_reached(expiry: RecordExpiry, now: u64) -> bool {
        matches!(expiry, RecordExpiry::At(at) if now >= at)
    }

    /// The Paykit member every path that signs the app's own record puts, with `endpoints`
    /// as the public endpoints wanted: the capabilities this app's payment method pays, and
    /// no public endpoints at all once the app doesn't publish them, without
    /// `paykit_request` or not publishing its record (Paykit spec 4, Paykit wire 3.2).
    fn own_member_with(
        &self,
        endpoints: BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
    ) -> PaykitMember {
        let publishes = holds_public_endpoints(self.app.cert(), self.app.publishes_record());
        let endpoint_identifiers = self.capability_snapshot.clone().unwrap_or_else(|| {
            capabilities_for(self.rail.as_ref(), holds_capabilities(self.app.cert()))
        });
        PaykitMember {
            endpoint_identifiers,
            public_endpoints: if publishes {
                endpoints
            } else {
                BTreeMap::new()
            },
        }
    }

    /// The Paykit member of the record the app holds, empty when it has none.
    fn own_member(&self) -> Result<PaykitMember> {
        let record = AppRecord::verify(self.app.record(), &self.identity())
            .map_err(|_| PaykitError::corrupt())?;
        PaykitMember::read(&record).map_err(|_| PaykitError::corrupt())
    }

    async fn put_member(
        &mut self,
        member: PaykitMember,
        desired: ProtocolRecord,
        expiry: RecordExpiry,
    ) -> Result<Publication> {
        let expiry = if member.public_endpoints.is_empty() {
            RecordExpiry::Never
        } else {
            expiry
        };
        let previous = PaykitMember::from_member(desired.member.as_ref())?;
        let value = if previous == member {
            desired.member
        } else {
            (member != PaykitMember::default()).then(|| member.to_member())
        };
        let result = self
            .app
            .set_protocol_member_with_expiry(PROTOCOL_NAME, value.clone(), expiry)
            .await;
        self.note_own_record_write(&result);
        if let Err(error @ AppError::Refused(_)) = &result {
            let kept = self.desired_record().await?;
            if kept.pending && kept.member == value && kept.expiry == expiry {
                return Err(PaykitError::PublicEndpointChangePending(error.clone()));
            }
        }
        Ok(result?)
    }

    /// After a client call that can change the own record (a write of it, a delivery, or a
    /// read that takes another device's record or a renewal) ends with the store unable to
    /// tell whether it was applied, the record this handle holds may not be the one stored:
    /// note it, so no edit builds on it until the app is loaded again.
    pub(crate) fn note_own_record_write<T>(&mut self, result: &std::result::Result<T, AppError>) {
        if matches!(result, Err(AppError::Store(StoreError::Unknown))) {
            self.own_record_unknown = true;
        }
    }

    /// Refuse an edit of the own record while the record this handle holds may not be the one
    /// stored ([`Paykit::set_public_endpoints`]).
    fn own_record_known(&self) -> Result<()> {
        if self.own_record_unknown {
            return Err(Refusal::NotNow(
                "the store couldn't tell whether the last write of the app's record was \
                 applied: load the app again (Paykit::load) before editing its record"
                    .into(),
            )
            .into());
        }
        Ok(())
    }

    /// Whether less than half of the held record's trusted lifetime is left on the device
    /// clock or the highest verified service time (module docs).
    fn renewal_due(&self, now: u64) -> Result<bool> {
        let record = AppRecord::verify(self.app.record(), &self.identity())
            .map_err(|_| PaykitError::corrupt())?;
        let max_age_s = self.app.own_identity().record_max_age_ms() / 1000;
        let updated = record.updated_at();
        let lifetime = record.trusted_until(max_age_s).saturating_sub(updated);
        Ok(now > updated.saturating_add(lifetime / 2))
    }
}

/// Whether an app under `cert` holds public endpoints: one holding `paykit_request` that
/// publishes its record (module docs). The one rule every path that builds the own member
/// follows, the renewal's included ([`member_under`]).
pub(crate) fn holds_public_endpoints(cert: &AppCert, publishes_record: bool) -> bool {
    publishes_record
        && paykit_lib::scopes::holds(cert.scopes(), &paykit_lib::scopes::PaykitScope::Request)
}

/// Whether an app under `cert` lists capabilities: one holding `paykit_pay` (Paykit wire
/// 3.2).
pub(crate) fn holds_capabilities(cert: &AppCert) -> bool {
    paykit_lib::scopes::holds(cert.scopes(), &paykit_lib::scopes::PaykitScope::Pay)
}

/// Paykit's member of the record signed under `cert`, a renewal the app takes, and its
/// expiry ask, from `member` and `ask`, the ones the record has now (Paykit spec, "What this pin adopts",
/// "Renewing a certificate"): the public endpoints go when the app under `cert` holds none
/// ([`holds_public_endpoints`]), the capabilities when it lists none
/// ([`holds_capabilities`]), and the ask is for no expiry once no public endpoint is left.
/// Everything else of the member stays as it is, fields this version doesn't read included,
/// and a member with nothing left is removed. Otherwise the member and its ask are unchanged.
pub(crate) fn member_under(
    member: Option<&serde_json::Map<String, serde_json::Value>>,
    ask: RecordExpiry,
    cert: &AppCert,
    publishes_record: Option<bool>,
) -> (
    Option<serde_json::Map<String, serde_json::Value>>,
    RecordExpiry,
) {
    // No member: no ask either, so an ask left over can't keep a later record from signing.
    let Some(member) = member else {
        return (None, RecordExpiry::Never);
    };
    let mut out = member.clone();
    // The publishing setting strips endpoints only when Paykit was told the app doesn't
    // publish; not told, the scopes alone decide.
    if !holds_public_endpoints(cert, publishes_record.unwrap_or(true)) {
        out.remove("public_endpoints");
    }
    if !holds_capabilities(cert) {
        out.remove("capabilities");
    }
    // Endpoints are left only as a non-empty object; anything else holds none.
    let endpoints_left = matches!(
        out.get("public_endpoints"),
        Some(serde_json::Value::Object(endpoints)) if !endpoints.is_empty()
    );
    // A retained member keeps the protocol's own cutoff through certificate renewal.
    // An app with no Paykit deadline leaves the other protocols' requests to the client.
    let ask = if endpoints_left {
        ask
    } else {
        RecordExpiry::Never
    };
    if out.is_empty() {
        return (None, RecordExpiry::Never);
    }
    (Some(out), ask)
}

/// The sorted, distinct identifiers the method pays under this certificate (Paykit wire 3.2).
pub(crate) fn capabilities_for(rail: &dyn Rail, pays: bool) -> Vec<PaymentEndpointIdentifier> {
    if !pays {
        return Vec::new();
    }
    let mut ids = rail.endpoint_identifiers();
    ids.sort();
    ids.dedup();
    ids
}

/// Check the first capabilities against the approved generation and the initial
/// record's null deadline before joining. Later changes are checked by the Switch
/// client against the resolved record, including the other protocols' actual members.
///
/// # Errors
///
/// A client usage refusal if the initial app record cannot fit in an `app.put`.
pub(crate) fn check_join_capabilities(
    mut body: AppRecordBody,
    ids: &[PaymentEndpointIdentifier],
    generation: u64,
) -> Result<()> {
    let refused = |error: pubky_switch_core::Error| {
        PaykitError::from(AppError::from(pubky_switch_client::app::Refusal::Usage(
            format!("the app record: {error}"),
        )))
    };
    if !ids.is_empty() {
        let member = PaykitMember {
            endpoint_identifiers: ids.to_vec(),
            public_endpoints: BTreeMap::new(),
        };
        body.protocols
            .insert(PROTOCOL_NAME.to_owned(), member.to_member());
    }
    let record = SignedDoc::from_parts(&body.to_json().map_err(refused)?, &[0; 64]);
    sign_batch(
        &Measure(body.signing_key),
        &body.identity,
        Some(body.app_id),
        generation,
        [0; 16],
        vec![(Vec::new(), OpKind::AppPut { record })],
    )
    .map_err(refused)?;
    Ok(())
}

/// A writer that only measures what it signs: the app's signing key as its public key, and
/// a placeholder of a signature's length for each signature.
struct Measure([u8; 32]);

impl Signs for Measure {
    fn public_key(&self) -> [u8; 32] {
        self.0
    }

    fn sign_fields(&self, _label: &str, _fields: &[&[u8]]) -> [u8; 64] {
        [0; 64]
    }
}

#[cfg(test)]
mod renewal_tests {
    use pubky_switch_client::app::Protocol;
    use serde_json::json;

    use super::*;
    use crate::testkit::{object, Party};

    fn cert(scopes: &[&str]) -> AppCert {
        Party::new("bob", "shop", scopes).record(&[]).cert().clone()
    }

    #[test]
    fn test_member_under_without_a_member_asks_for_no_expiry() {
        let cert = cert(&["paykit_request", "paykit_pay"]);
        assert_eq!(
            member_under(None, RecordExpiry::At(5_000), &cert, Some(true)),
            (None, RecordExpiry::Never)
        );
    }

    #[test]
    fn test_member_under_endpoints_only_as_a_non_empty_object() {
        let cert = cert(&["paykit_request", "paykit_pay"]);
        for value in [json!([]), json!({}), json!(null), json!("lno1")] {
            let member = object(json!({ "public_endpoints": value, "note": 1 }));
            let (_, ask) = member_under(Some(&member), RecordExpiry::At(5_000), &cert, Some(true));
            assert_eq!(ask, RecordExpiry::Never, "{member:?}");
        }
        let member = object(json!({ "public_endpoints": { "btc-lightning-bolt12": "lno1" } }));
        assert_eq!(
            member_under(Some(&member), RecordExpiry::At(5_000), &cert, Some(true)),
            (Some(member.clone()), RecordExpiry::At(5_000))
        );
        // Public endpoints can have no Paykit deadline.
        assert_eq!(
            member_under(Some(&member), RecordExpiry::Never, &cert, Some(true)).1,
            RecordExpiry::Never
        );
    }

    #[test]
    fn test_member_for_of_a_host_built_protocol_keeps_an_endpoint_its_scopes_allow() {
        let cert = cert(&["paykit_request", "paykit_pay"]);
        let member = object(json!({ "public_endpoints": { "btc-lightning-bolt12": "lno1" } }));
        let ask = RecordExpiry::At(5_000);
        // Not told whether the app publishes, the scopes alone decide.
        let protocol = crate::protocol::PaykitProtocol::new();
        assert_eq!(
            protocol.member_for(Some(&member), ask, &cert),
            (Some(member.clone()), ask)
        );
        // Told it doesn't, the endpoints go.
        assert_eq!(
            member_under(Some(&member), ask, &cert, Some(false)),
            (None, RecordExpiry::Never)
        );
    }
}
