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
//! [`Paykit::own_public_endpoints`] lists the ones the app's record holds. Every change
//! signs a new record, which the Switch client keeps, delivers to the own switch with
//! `app.put` and then writes to the own Homeserver, in that order and in the same bytes
//! (Switch spec 4.2, [`pubky_switch_client::app::App::set_protocol_member_with_lifetime`]). A delivery
//! or a Homeserver write that fails stays owed by the client and goes again at the next
//! read; until then readers of the Homeserver see the record published before. The client's
//! record is the only copy of the public endpoints: Paykit keeps none of its own, and
//! nothing is issued, reserved or released for them, since a public endpoint is reusable.
//!
//! Only an app holding `paykit_request` itself, configured to publish its record
//! ([`pubky_switch_client::app::AppConfig::publishes_record`]), publishes public endpoints:
//! a payment to one carries no label, so its proof reaches only the payee's apps holding
//! `paykit_request` (Paykit spec 4, 8.4). An app that loses `paykit_request`, or stops
//! publishing, can still remove endpoints, and at its next receive its held record carries
//! none. With publishing disabled, the client writes nothing to the Homeserver: withdraw
//! endpoints before disabling it to update that copy. Stale copies remain payable until
//! expiry or until a payer sees the app revoked (Paykit spec 4).
//!
//! **Expiry and renewal.** A record with public endpoints carries `expires_at`, at its
//! `updated_at` plus the shorter of [`RECORD_LIFETIME_MS`] and the identity's
//! `record_max_age`, which readers would cut it to anyway. The client derives that expiry
//! at the actual signing timestamp, including after clock rollback or a conflict retry;
//! one without public endpoints asks for none (Paykit wire 3.2). At every receive, once
//! less than half of the record's trusted lifetime is left on the later of the device
//! clock and verified service time, the app renews it with the client's
//! [`renew_record_with_lifetime`](pubky_switch_client::app::App::renew_record_with_lifetime).
//! The trusted lifetime ends at the earlier of `expires_at` and `updated_at` plus
//! `record_max_age`. A record with more left isn't signed again. A renewal the switch
//! hasn't accepted yet is
//! the record the app holds, pending, and the client sends it again at the next read: the
//! check reads the record held, so nothing counts as renewed that isn't kept.
//!
//! **Capabilities.** `capabilities.endpoint_identifiers` lists what this app's payment
//! method pays ([`crate::Rail::endpoint_identifiers`]) at an app holding `paykit_pay`, and
//! nothing at one without (Paykit wire 3.2). At the join and at every receive the record is
//! brought to that list when it differs, and
//! [`Paykit::update_own_record`] does so at once.
//!
//! Security: the record is public by design, readable by anyone on the Homeserver (Paykit
//! spec 4, "What is public"); it goes to no counterparty, so no deny list gates it. Every
//! identifier and payload it carries went through the validated constructors
//! ([`PaymentEndpointIdentifier`], [`PaymentEndpointPayload`]); a payer reading it still
//! checks the signature, the app's standing and the record's lifetime before paying.

use std::collections::BTreeMap;

use paykit_lib::app_record::{PaykitMember, PROTOCOL_NAME};
use paykit_lib::{PaymentEndpointIdentifier, PaymentEndpointPayload};
use pubky_switch_client::app::Publication;
use pubky_switch_core::records::AppRecord;

use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;

/// The lifetime of a record with public endpoints the SDK signs: 30 days, the recommended
/// most, unless the identity's `record_max_age` is shorter (Paykit spec 4, Paykit wire 3.2).
pub const RECORD_LIFETIME_MS: u64 = 30 * 86_400_000;

/// The most public endpoints the app publishes: a payer's read takes at most this many over
/// all of a payee's apps, so a record with more would never be paid (Paykit spec 4).
pub const MAX_OWN_PUBLIC_ENDPOINTS: usize = crate::app_records::MAX_PUBLIC_ENDPOINTS;

impl Paykit {
    /// The public endpoints the app's record holds now, by Payment Endpoint Identifier: the
    /// latest record the app signed, which the switch may not have accepted yet.
    ///
    /// # Errors
    ///
    /// A corrupt store when the record held doesn't verify or its Paykit member doesn't
    /// read.
    pub fn own_public_endpoints(
        &self,
    ) -> Result<BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>> {
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
        Box::pin(self.first_read_before_writing()).await?;
        let mut endpoints = self.own_public_endpoints()?;
        endpoints.insert(endpoint, payload);
        self.set_public_endpoints(endpoints).await
    }

    /// Stop publishing the app's public endpoint `endpoint`; one the record doesn't hold is
    /// removed already, and nothing is signed.
    ///
    /// # Errors
    ///
    /// As [`Paykit::set_public_endpoints`].
    pub async fn remove_public_endpoint(
        &mut self,
        endpoint: &PaymentEndpointIdentifier,
    ) -> Result<Publication> {
        Box::pin(self.first_read_before_writing()).await?;
        let mut endpoints = self.own_public_endpoints()?;
        endpoints.remove(endpoint);
        self.set_public_endpoints(endpoints).await
    }

    /// Publish exactly `endpoints` as the app's public endpoints, none to publish none
    /// (module docs): a new record is signed, kept, delivered to the own switch and written
    /// to the own Homeserver. When the record already holds them, its saved lifetime
    /// policy is kept and owed delivery finishes. An unaccepted restored record is signed
    /// anew; accepted and pending records retain their bytes.
    ///
    /// # Errors
    ///
    /// [`Refusal::CertificateRefused`]; [`Refusal::IdentityUnavailable`] while a restore's
    /// first read cannot confirm the own identity; the client's first-read errors;
    /// [`Refusal::ScopeMissing`] without `paykit_request`,
    /// unless the call only removes endpoints; [`Refusal::Usage`] for an app that doesn't
    /// publish its record, unless the call only removes endpoints, or more than
    /// [`MAX_OWN_PUBLIC_ENDPOINTS`] endpoints. The client's refusal for a record the switch
    /// would refuse, such as one over its size bound. Nothing is signed or kept for these
    /// refusals. After the record is kept, the client's delivery error leaves the record
    /// owed for the next read. A Homeserver failure is returned as [`Publication::Owed`] or
    /// [`Publication::Unchecked`], and the client retries it. Removing endpoints while
    /// publishing is disabled changes the held record without writing to the Homeserver.
    pub async fn set_public_endpoints(
        &mut self,
        endpoints: BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
    ) -> Result<Publication> {
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        // Fewer endpoints, none included, are always the app's to publish: an app that lost
        // `paykit_request` or stopped publishing can still clear its held endpoints.
        let held = self.own_public_endpoints()?;
        let removes_only = endpoints
            .iter()
            .all(|(id, payload)| held.get(id) == Some(payload));
        if !self.holds("paykit_request") && !removes_only {
            return Err(Refusal::ScopeMissing("paykit_request".into()).into());
        }
        if !removes_only && !self.config.app.publishes_record {
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
        let member = PaykitMember {
            endpoint_identifiers: self.capabilities(),
            public_endpoints: endpoints,
        };
        Box::pin(self.put_member(member)).await
    }

    /// Bring the app's record up to date now (module docs), as each receive and the join do:
    /// for a host whose payment method changed what it pays, so the record lists it at once.
    ///
    /// # Errors
    ///
    /// [`Refusal::CertificateRefused`]; [`Refusal::IdentityUnavailable`] while a restore's
    /// first read cannot confirm the own identity; the client's refusal, store failure,
    /// alarm, first-read or delivery error. A record refused before it is kept leaves the held record unchanged.
    /// A delivery that fails stays owed by the client and goes again at the next read.
    pub async fn update_own_record(&mut self) -> Result<()> {
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        Box::pin(self.tend_own_record()).await
    }

    /// Bring the app's record up to date at a receive (module docs): its capabilities to
    /// what the payment method pays, and a record with public endpoints renewed once less
    /// than half of its lifetime is left. A delivery that fails stays owed by the client,
    /// which sends it again at a later read.
    ///
    /// # Errors
    ///
    /// The client's refusal, store failure, alarm or delivery error.
    pub(crate) async fn tend_own_record(&mut self) -> Result<()> {
        let held = self.own_member()?;
        // An app that no longer holds `paykit_request`, or no longer publishes its record,
        // signs its next record without public endpoints (Paykit wire 3.2).
        let publishes = self.holds("paykit_request") && self.config.app.publishes_record;
        let wanted = PaykitMember {
            endpoint_identifiers: self.capabilities(),
            public_endpoints: if publishes {
                held.public_endpoints.clone()
            } else {
                BTreeMap::new()
            },
        };
        if wanted != held {
            Box::pin(self.put_member(wanted)).await.map(|_| ())
        } else if !held.public_endpoints.is_empty() && self.renewal_due()? {
            let lifetime = Some(self.record_lifetime());
            Box::pin(self.app.renew_record_with_lifetime(PROTOCOL_NAME, lifetime))
                .await
                .map(|_| ())
                .map_err(PaykitError::from)
        } else {
            Ok(())
        }
    }

    /// The Paykit member of the record the app holds, empty when it has none.
    fn own_member(&self) -> Result<PaykitMember> {
        let record = AppRecord::verify(self.app.record(), &self.identity())
            .map_err(|_| PaykitError::corrupt())?;
        PaykitMember::read(&record).map_err(|_| PaykitError::corrupt())
    }

    /// `capabilities.endpoint_identifiers` for this app: what its payment method pays, at an
    /// app holding `paykit_pay`, and nothing at one without (Paykit wire 3.2).
    fn capabilities(&self) -> Vec<PaymentEndpointIdentifier> {
        if !self.holds("paykit_pay") {
            return Vec::new();
        }
        let mut ids = self.rail.endpoint_identifiers();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Sign `member` into the app's record, with `expires_at` when it carries public
    /// endpoints and none when it doesn't, and send it as the client orders (module docs).
    async fn put_member(&mut self, member: PaykitMember) -> Result<Publication> {
        let lifetime = (!member.public_endpoints.is_empty()).then(|| self.record_lifetime());
        let value = (member != PaykitMember::default()).then(|| member.to_member());
        Ok(self
            .app
            .set_protocol_member_with_lifetime(PROTOCOL_NAME, value, lifetime)
            .await?)
    }

    /// The requested lifetime; the client caps it again at the current identity limit
    /// after readiness and binds it to the exact timestamp it signs (Paykit wire 3.2).
    fn record_lifetime(&self) -> u64 {
        RECORD_LIFETIME_MS.min(self.app.own_identity().record_max_age_ms())
    }

    /// Whether less than half of the held record's trusted lifetime is left on the device
    /// clock or the highest verified service time (module docs).
    fn renewal_due(&self) -> Result<bool> {
        let record = AppRecord::verify(self.app.record(), &self.identity())
            .map_err(|_| PaykitError::corrupt())?;
        let max_age_s = self.app.own_identity().record_max_age_ms() / 1000;
        let updated = record.updated_at();
        let lifetime = record.trusted_until(max_age_s).saturating_sub(updated);
        let now = self.now().max(self.app.highest_service_time().unwrap_or(0));
        Ok(now > updated.saturating_add(lifetime / 2))
    }
}
