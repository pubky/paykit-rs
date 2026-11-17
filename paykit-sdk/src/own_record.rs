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
//! publishing, holds no public endpoints: every record it signs from then on carries none,
//! whichever path signs it (a removal, the upkeep at a receive or an update, or the join),
//! so a removal there withdraws them all. With publishing disabled, the client writes
//! nothing to the Homeserver: withdraw endpoints before disabling it to update that copy. Stale copies remain payable until
//! expiry or until a payer sees the app revoked (Paykit spec 4).
//!
//! **Expiry and renewal.** A record with public endpoints carries `expires_at`, at its
//! `updated_at` plus the shorter of [`RECORD_LIFETIME_MS`] and the identity's
//! `record_max_age`, which readers would cut it to anyway: that is what Paykit asks for, and
//! the `expires_at` signed is the earliest any protocol of the app asks for. The client derives that expiry
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
use pubky_switch_client::app::{AppError, Publication};
use pubky_switch_client::error::StoreError;
use pubky_switch_core::json::MAX_INT;
use pubky_switch_core::ops::{sign_batch, OpKind};
use pubky_switch_core::records::{AppRecord, AppRecordBody};
use pubky_switch_core::sig::SignedDoc;
use pubky_switch_core::signing::Signs;
use pubky_switch_core::time::MAX_TIME_MS;

use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::rail::Rail;
use crate::state::key;

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
        self.own_record_known()?;
        Box::pin(self.first_read_before_writing()).await?;
        let mut endpoints = self.own_public_endpoints()?;
        endpoints.insert(endpoint, payload);
        self.set_public_endpoints(endpoints).await
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
        self.own_record_known()?;
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
    /// [`Refusal::NotNow`] after a write of the own record whose store result was unknown,
    /// until the app is loaded again ([`Paykit::load`]): the record this handle holds may not
    /// be the one stored, and an edit built on it could publish an endpoint withdrawn;
    /// [`Refusal::ScopeMissing`] without `paykit_request`,
    /// unless the call only removes endpoints; [`Refusal::Usage`] for an app that doesn't
    /// publish its record, unless the call only removes endpoints, or more than
    /// [`MAX_OWN_PUBLIC_ENDPOINTS`] endpoints. The client's refusal for a record the switch
    /// would refuse, such as one over its size bound. Nothing is signed or kept for these
    /// refusals. After the record is kept, the client's delivery error leaves the record
    /// owed for the next read. A Homeserver failure is returned as [`Publication::Owed`] or
    /// [`Publication::Unchecked`], and the client retries it. Removing endpoints while the
    /// app doesn't publish signs a held record with none, as the upkeep would, without
    /// writing to the Homeserver.
    pub async fn set_public_endpoints(
        &mut self,
        endpoints: BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
    ) -> Result<Publication> {
        self.own_record_known()?;
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
        let member = self.own_member_with(endpoints)?;
        Box::pin(self.put_member(member)).await
    }

    /// Bring the app's record up to date now (module docs), as each receive and the join do:
    /// for a host whose payment method changed what it pays, so the record lists it at once.
    /// When the record held is already up to date, what is owed of it goes instead: its
    /// delivery to the own switch, as after one that failed once the record was kept, and its
    /// Homeserver write ([`pubky_switch_client::app::App::publish_record`]).
    ///
    /// Returns where the record stands on the Homeserver after the call, as
    /// [`Paykit::set_public_endpoints`] does: [`Publication::Owed`] when the Homeserver write
    /// failed, which the client retries at the next read or update, and
    /// [`Publication::Unchecked`] when the read that checks the Homeserver copy failed.
    ///
    /// # Errors
    ///
    /// [`Refusal::CertificateRefused`]; [`Refusal::IdentityUnavailable`] while a restore's
    /// first read cannot confirm the own identity; the client's refusal, store failure,
    /// alarm, first-read or delivery error; [`Refusal::NotNow`] after an unknown store
    /// result, until the app is loaded again, as [`Paykit::set_public_endpoints`] says. A
    /// record refused before it is kept leaves the
    /// held record unchanged, and so does a capabilities list the record can't carry
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
    /// capabilities list the record can't carry ([`capabilities_for`]).
    pub(crate) async fn tend_own_record(&mut self) -> Result<Option<Publication>> {
        // Nothing is signed while the record this handle holds may not be the one stored:
        // the next load reads it again (`own_record_known`).
        if self.own_record_unknown {
            return Ok(None);
        }
        let held = self.own_member()?;
        let wanted = self.own_member_with(held.public_endpoints.clone())?;
        let publication = if wanted != held {
            Box::pin(self.put_member(wanted)).await?
        } else if held.public_endpoints.is_empty() && self.withdrawal_cleanup_owed().await? {
            // The second step of a last withdrawal is owed, the withdrawal itself signed.
            match Box::pin(self.finish_withdrawal()).await? {
                Some(publication) => publication,
                None => return Ok(None),
            }
        } else if !held.public_endpoints.is_empty() && self.renewal_due()? {
            let lifetime = Some(self.record_lifetime());
            let renewed =
                Box::pin(self.app.renew_record_with_lifetime(PROTOCOL_NAME, lifetime)).await;
            self.note_own_record_write(&renewed);
            renewed?
        } else {
            // The member unchanged still goes to the client: a restored record the switch
            // never accepted, such as a withdrawal whose `app.put` failed before the backup,
            // is signed anew, and not left for the accepted record before it to be
            // published. An accepted or pending record keeps its bytes. The member goes as
            // the record holds it, not rebuilt from what Paykit reads of it, so a member
            // carrying what this version doesn't read, or leaving out an optional part, is
            // the same member to the client and is never signed again for that.
            let record = AppRecord::verify(self.app.record(), &self.identity())
                .map_err(|_| PaykitError::corrupt())?;
            let value = record.body().protocols.get(PROTOCOL_NAME).cloned();
            let lifetime = (!held.public_endpoints.is_empty()).then(|| self.record_lifetime());
            Box::pin(self.put_member_value(value, lifetime)).await?;
            return Ok(None);
        };
        Ok(Some(publication))
    }

    /// The Paykit member every path that signs the app's own record puts, with `endpoints`
    /// as the public endpoints wanted: the capabilities this app's payment method pays, and
    /// no public endpoints at all once the app doesn't publish them, without
    /// `paykit_request` or not publishing its record (Paykit spec 4, Paykit wire 3.2).
    fn own_member_with(
        &self,
        endpoints: BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
    ) -> Result<PaykitMember> {
        let publishes = self.holds("paykit_request") && self.config.app.publishes_record;
        Ok(PaykitMember {
            endpoint_identifiers: self.capabilities()?,
            public_endpoints: if publishes {
                endpoints
            } else {
                BTreeMap::new()
            },
        })
    }

    /// The Paykit member of the record the app holds, empty when it has none.
    fn own_member(&self) -> Result<PaykitMember> {
        let record = AppRecord::verify(self.app.record(), &self.identity())
            .map_err(|_| PaykitError::corrupt())?;
        PaykitMember::read(&record).map_err(|_| PaykitError::corrupt())
    }

    /// `capabilities.endpoint_identifiers` for this app, checked against the record it
    /// holds ([`capabilities_for`]).
    fn capabilities(&self) -> Result<Vec<PaymentEndpointIdentifier>> {
        let held = AppRecord::verify(self.app.record(), &self.identity())
            .map_err(|_| PaykitError::corrupt())?;
        capabilities_for(self.rail.as_ref(), self.holds("paykit_pay"), held.body())
    }

    /// Sign `member` into the app's record, with `expires_at` when it carries public
    /// endpoints and none when it doesn't, and send it as the client orders (module docs).
    /// Returns where the last record the call signed stands on the Homeserver.
    ///
    /// The last withdrawal, a member without public endpoints over one with some, goes in two
    /// steps, so the member and its expiry ask are never apart: the client keeps a protocol's
    /// ask even when another protocol's lapsed expiry refuses the signing, so asking for no
    /// expiry with the change itself could leave the endpoints held with none, a member no
    /// reader takes (Paykit wire 3.2). First Paykit's durable mark that the second step is
    /// owed is written, and the withdrawal is signed with Paykit's lifetime ask kept, the
    /// member staying in the record (with the capabilities of an app that pays, or empty).
    /// Then the second step asks for no expiry ([`Paykit::finish_withdrawal`]). Setting
    /// endpoints again clears the mark: the lifetime ask is wanted again.
    async fn put_member(&mut self, member: PaykitMember) -> Result<Publication> {
        self.own_record_known()?;
        let withdraws_last =
            member.public_endpoints.is_empty() && !self.own_member()?.public_endpoints.is_empty();
        if withdraws_last {
            self.local_put(key::WITHDRAWAL_CLEANUP_OWED, &true).await?;
        } else if !member.public_endpoints.is_empty() && self.withdrawal_cleanup_owed().await? {
            self.local_delete(key::WITHDRAWAL_CLEANUP_OWED).await?;
        }
        let lifetime =
            (!member.public_endpoints.is_empty() || withdraws_last).then(|| self.record_lifetime());
        // A member that changes nothing goes as the record holds it, so what this version
        // doesn't read is kept and nothing is signed for it. The client asks for no expiry
        // for a member it removes: through the last withdrawal the member stays.
        let value = if member == self.own_member()? {
            AppRecord::verify(self.app.record(), &self.identity())
                .map_err(|_| PaykitError::corrupt())?
                .body()
                .protocols
                .get(PROTOCOL_NAME)
                .cloned()
        } else {
            (member != PaykitMember::default() || withdraws_last).then(|| member.to_member())
        };
        let publication = Box::pin(self.put_member_value(value, lifetime)).await?;
        if withdraws_last {
            if let Some(cleaned) = Box::pin(self.finish_withdrawal()).await? {
                return Ok(cleaned);
            }
        }
        Ok(publication)
    }

    /// Whether the second step of a last withdrawal is owed ([`Paykit::put_member`]).
    async fn withdrawal_cleanup_owed(&self) -> Result<bool> {
        Ok(self
            .local_get::<bool>(key::WITHDRAWAL_CLEANUP_OWED)
            .await?
            .unwrap_or(false))
    }

    /// The second step of a last withdrawal: Paykit asks for no expiry, the member's bytes
    /// kept, which signs the record anew (Paykit wire 3.2: a record without public endpoints
    /// may write `null`). It runs only while the mark is set and the record held has no
    /// public endpoints, never on what the record looks like: another protocol's expiry or
    /// a member of fields this version doesn't read decides nothing. The mark goes once the
    /// client has signed the record, whatever its delivery did; a result the store can't
    /// tell keeps it and stops the handle's edits until a load (`own_record_known`).
    /// `Ok(None)` when the client refused before signing, as while another protocol's
    /// expiry has lapsed: nothing changed, the mark stays and a later upkeep tries again.
    async fn finish_withdrawal(&mut self) -> Result<Option<Publication>> {
        self.own_record_known()?;
        let before = self.app.record().clone();
        let renewed = Box::pin(self.app.renew_record_with_lifetime(PROTOCOL_NAME, None)).await;
        self.note_own_record_write(&renewed);
        if self.own_record_unknown {
            return Err(renewed
                .err()
                .map_or_else(PaykitError::corrupt, PaykitError::from));
        }
        if self.app.record() != &before {
            self.local_delete(key::WITHDRAWAL_CLEANUP_OWED).await?;
        }
        match renewed {
            Ok(publication) => Ok(Some(publication)),
            Err(AppError::Refused(_)) if self.app.record() == &before => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Put `value` as the record's Paykit member, `None` for none, asking for `lifetime`.
    async fn put_member_value(
        &mut self,
        value: Option<serde_json::Map<String, serde_json::Value>>,
        lifetime: Option<u64>,
    ) -> Result<Publication> {
        self.own_record_known()?;
        let put = self
            .app
            .set_protocol_member_with_lifetime(PROTOCOL_NAME, value, lifetime)
            .await;
        self.note_own_record_write(&put);
        Ok(put?)
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

/// `capabilities.endpoint_identifiers` for an app whose record is `base`: what its payment
/// method pays, sorted and without repeats, at an app holding `paykit_pay` (`pays`), and
/// nothing at one without (Paykit wire 3.2). [`Rail::endpoint_identifiers`] enters the SDK
/// here alone, so a list the record can't carry is refused the same way at the join, a
/// receive, [`Paykit::update_own_record`] and [`Paykit::set_public_endpoints`].
///
/// # Errors
///
/// The client's `Usage` refusal for a list that `base`, with it as its only Paykit member,
/// can't carry ([`check_member_fits`]).
pub(crate) fn capabilities_for(
    rail: &dyn Rail,
    pays: bool,
    base: &AppRecordBody,
) -> Result<Vec<PaymentEndpointIdentifier>> {
    if !pays {
        return Ok(Vec::new());
    }
    let mut ids = rail.endpoint_identifiers();
    ids.sort();
    ids.dedup();
    check_member_fits(
        base,
        &PaykitMember {
            endpoint_identifiers: ids.clone(),
            public_endpoints: BTreeMap::new(),
        },
    )?;
    Ok(ids)
}

/// Refuse `member` when the record `base`, with it as its Paykit member, would make an
/// `app.put` over the Switch's cap on a signed operation (Switch wire 6.1): the cap at
/// which the client refuses the record before signing or keeping anything. The record and
/// its `app.put` are built by the same core functions the client uses, with what the client
/// fills in at signing at its widest (the times and the generation) and a placeholder for
/// each signature, which is always 64 bytes. So a member this passes is one the client
/// signs, for a record with the same certificate, label and no wider `accepts`.
///
/// # Errors
///
/// The client's `Usage` refusal, in the client's words, for a record over the cap.
fn check_member_fits(base: &AppRecordBody, member: &PaykitMember) -> Result<()> {
    let refused = |error: pubky_switch_core::Error| {
        PaykitError::from(AppError::from(pubky_switch_client::app::Refusal::Usage(
            format!("the app record: {error}"),
        )))
    };
    let mut body = base.clone();
    if *member == PaykitMember::default() {
        body.protocols.remove(PROTOCOL_NAME);
    } else {
        body.protocols
            .insert(PROTOCOL_NAME.to_owned(), member.to_member());
    }
    body.updated_at = MAX_TIME_MS;
    body.expires_at = Some(MAX_TIME_MS);
    let record = SignedDoc::from_parts(&body.to_json().map_err(refused)?, &[0; 64]);
    sign_batch(
        &Measure(body.signing_key),
        &body.identity,
        Some(body.app_id),
        MAX_INT,
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
