//! The app's own record: Paykit's member `protocols.paykit`, with the app's public Payment
//! Endpoints and the Payment Endpoint Identifiers its payment method pays (Paykit spec 4,
//! Paykit wire 3.2).
//!
//! After a restore, public mutations wait for the identity-confirmed first read, and
//! scheduled upkeep runs only after a completed read (Dispatch spec 12.4).
//!
//! **Publishing.** Each public endpoint is one entry of the record's `public_endpoints`, by
//! its Payment Endpoint Identifier; [`Paykit::set_public_endpoint`],
//! [`Paykit::remove_public_endpoint`] and [`Paykit::set_public_endpoints`] change them, and
//! [`Paykit::own_public_endpoints`] lists the ones the app's record holds. Once a change can be signed,
//! the Dispatch client keeps the new record, delivers it to the own Dispatch with
//! `app.put` and then writes to the own Homeserver, in that order and in the same bytes
//! (Dispatch spec 4.2, [`pubky_dispatch_client::app::App::set_protocol_member_with_expiry`]). A delivery
//! or a Homeserver write that fails stays owed by the client and goes again at the next
//! read; until then readers of the Homeserver see the record published before. The client's
//! signed record shows the last signed endpoints. The client can also retain an unsigned
//! member edit when a sibling protocol blocks signing. Paykit keeps no separate copy, and
//! nothing is issued, reserved or released for them, since a public endpoint is reusable.
//!
//! Only an app holding `paykit_request` itself, configured to publish its record
//! ([`pubky_dispatch_client::app::AppConfig::publishes_record`]), publishes public endpoints:
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
//! would make its `app.put` larger than the Dispatch's cap on a signed operation (Dispatch wire
//! 6.1), is refused the same way wherever it is read: at the join before anything is
//! written, and at a receive, an update or a publication before anything is signed.
//!
//! Security: the record is public by design, readable by anyone on the Homeserver (Paykit
//! spec 4, "What is public"); it goes to no counterparty, so no deny list gates it. Every
//! identifier and payload it carries went through the validated constructors
//! ([`PaymentEndpointIdentifier`], [`PaymentEndpointPayload`]); a payer reading it still
//! checks the signature, the app's standing and the record's lifetime before paying.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use paykit_lib::app_record::{PaykitMember, PROTOCOL_NAME};
use paykit_lib::{PaymentEndpointIdentifier, PaymentEndpointPayload};
use pubky_dispatch_client::app::{
    AppError, ProtocolRecord, Publication, RecordExpiry, Refusal as ClientRefusal,
};
use pubky_dispatch_client::error::StoreError;
use pubky_dispatch_core::ops::{sign_batch, OpKind};
use pubky_dispatch_core::records::{AppCert, AppRecord, AppRecordBody};
use pubky_dispatch_core::sig::SignedDoc;
use pubky_dispatch_core::signing::Signs;

use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::rail::Rail;
use crate::state::key;

/// The most public endpoints the app publishes: a payer's read takes at most this many over
/// all of a payee's apps, so a record with more would never be paid (Paykit spec 4).
pub const MAX_OWN_PUBLIC_ENDPOINTS: usize = crate::app_records::MAX_PUBLIC_ENDPOINTS;

impl Paykit {
    /// The public endpoints the app's record holds now, by Payment Endpoint Identifier: the
    /// latest record the app signed, which the Dispatch may not have accepted yet.
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
    /// there (module docs). The other public endpoints stay only while the app is eligible
    /// to publish and its existing offer has not reached its cutoff.
    ///
    /// # Errors
    ///
    /// As [`Paykit::set_public_endpoints`].
    pub async fn set_public_endpoint(
        &mut self,
        endpoint: PaymentEndpointIdentifier,
        payload: PaymentEndpointPayload,
    ) -> Result<Publication> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        Box::pin(self.edit_public_endpoint(endpoint, Some(payload))).await
    }

    /// Stop publishing the app's public endpoint `endpoint`; one the record doesn't hold is
    /// removed already. A record is still signed when something else of it is owed: the
    /// capabilities the payment method pays changed, or a restored record the Dispatch never
    /// accepted is signed anew ([`Paykit::set_public_endpoints`]).
    ///
    /// # Errors
    ///
    /// As [`Paykit::set_public_endpoints`].
    pub async fn remove_public_endpoint(
        &mut self,
        endpoint: &PaymentEndpointIdentifier,
    ) -> Result<Publication> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
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
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
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
        // What this call asks, against the record signed now, is kept before the client is
        // asked to save it: only to tell the host exactly what a held change was.
        let signed = self.own_member()?.public_endpoints;
        let asked = EndpointChange {
            withdrawn: signed
                .keys()
                .filter(|id| !endpoints.contains_key(*id))
                .map(|id| id.as_str().to_owned())
                .collect(),
            offered: endpoints
                .keys()
                .filter(|id| !signed.contains_key(*id))
                .map(|id| id.as_str().to_owned())
                .collect(),
            changed: endpoints
                .iter()
                .filter(|(id, payload)| signed.get(*id).is_some_and(|held| held != *payload))
                .map(|(id, _)| id.as_str().to_owned())
                .collect(),
        };
        if asked.is_empty() {
            self.local_delete(key::ENDPOINT_CHANGE).await?;
        } else {
            self.local_put(key::ENDPOINT_CHANGE, &asked).await?;
        }
        let member = self.own_member_with(endpoints);
        Box::pin(self.put_member(member, desired, expiry)).await
    }

    /// Bring the app's record up to date now (module docs), as each receive and the join do:
    /// for a host whose payment method changed what it pays, so the record lists it at once.
    /// When the record held is already up to date, what is owed of it goes instead: its
    /// delivery to the own Dispatch, as after one that failed once the record was kept, and its
    /// Homeserver write ([`pubky_dispatch_client::app::App::publish_record`]).
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
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        self.own_record_known()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        match Box::pin(self.tend_own_record()).await? {
            Tended::Signed(publication) => Ok(publication),
            Tended::Held | Tended::Unchanged(_) => {
                let published = Box::pin(self.app.publish_record()).await;
                self.note_own_record_write(&published);
                Ok(published?)
            }
        }
    }

    /// Bring the app's record up to date at a receive (module docs): its capabilities to
    /// what the payment method pays, and a record with public endpoints renewed once less
    /// than half of its lifetime is left. Returns what it did ([`Tended`]): where a record
    /// changed or renewed stands on the Homeserver, or the one held, which went to the client
    /// unchanged; or nothing tried, while the record held may not be the one stored. A
    /// delivery that fails stays owed by the client, which sends it again at a later read.
    /// The client saves an edit before it reads the service time to sign it: when that read
    /// fails, the edit stays saved and this run's later read signs it. A definite failure
    /// before the client saved the edit leaves the record held unchanged, so the next call
    /// finds it differs and puts it again. An unknown
    /// store result holds upkeep until the app is loaded again ([`Paykit::load`]).
    ///
    /// # Errors
    ///
    /// The client's refusal, store failure, alarm or delivery error; the refusal of a
    /// capabilities list the resolved record cannot carry (Dispatch wire 6.1).
    pub(crate) async fn tend_own_record(&mut self) -> Result<Tended> {
        // Nothing is signed while the record this handle holds may not be the one stored:
        // the next load reads it again (`own_record_known`).
        if self.own_record_unknown {
            return Ok(Tended::Held);
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
                .map(Tended::Signed);
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
                    return Err(Refusal::Usage(UNKNOWN_EXPIRY_POLICY.into()).into());
                }
            };
            self.note_own_record_write(&renewed);
            return Ok(Tended::Signed(renewed?));
        }
        // The unchanged-member call retains the exact policy and optional fields. A
        // restored record never accepted by the service still gets its required new delivery.
        let result = self
            .app
            .set_protocol_member_with_lifetime(PROTOCOL_NAME, desired.member, None)
            .await;
        self.note_own_record_write(&result);
        Ok(Tended::Unchanged(result?))
    }

    /// The upkeep at a receive ([`Paykit::tend_own_record`]), held to its own outcome: the
    /// record is the app's public description (Paykit spec 4), which no incoming message, record
    /// write or payment of this receive depends on, so a failure that is only the publisher's
    /// own leaves the record owed for the next receive or [`Paykit::update_own_record`], and the
    /// receive goes on. Returns that failure's reason for [`crate::ReceiveSummary`].
    ///
    /// The steps after the upkeep act under the app's standing as the read left it: its
    /// certificate, the newest own `identity.json` and a recovery notice. The client can change
    /// that standing within the upkeep and still fail it: a delivery whose answer names a later
    /// generation reads the newer `identity.json` (Dispatch wire 7.1), and a conflict with a later
    /// record of this app takes its renewal before fetching the scope keys (Dispatch spec 3.5).
    /// A failure after such a change ends the receive, as any failure did before, whatever it
    /// is ([`upkeep_outcome`]).
    ///
    /// # Errors
    ///
    /// Every failure [`own_record_left_owed`] doesn't name, and any failure after the upkeep
    /// changed the app's standing. While the store can't tell whether the last write of the
    /// record was applied, the upkeep isn't tried and the record is reported owed until the
    /// app is loaded again.
    pub(crate) async fn tend_own_record_at_receive(&mut self) -> Result<Option<String>> {
        let before = self.own_standing();
        // What a reload or a restore held aside is decided first. The order is safe: the step
        // signs nothing of Paykit's member, so the upkeep after it signs as it would without it.
        // A failing upkeep, such as one a sibling's expired policy refuses, can then no longer
        // leave a held change untold, nor keep it held, which would keep the record's
        // publication waiting. A failure of the step itself is the upkeep's.
        if let Err(error) = Box::pin(self.settle_held_record_edits()).await {
            return upkeep_outcome(error, &before, &self.own_standing());
        }
        match Box::pin(self.tend_own_record()).await {
            // Nothing is signed while the record this handle holds may not be the one stored,
            // so the record isn't brought up to date until the app is loaded again.
            Ok(Tended::Held) => Ok(Some(OWED_UNTIL_LOADED.to_string())),
            Ok(Tended::Signed(publication) | Tended::Unchanged(publication)) => {
                Ok(publication_owed(&publication).map(str::to_string))
            }
            Err(error) => upkeep_outcome(error, &before, &self.own_standing()),
        }
    }

    /// Decide what this app's record changes held aside become, once the restore's first
    /// read is complete (Paykit spec 4, "Changes saved and not yet signed"): a later run of
    /// the app or a restore holds a change the client saved and didn't sign aside for its
    /// protocol (Dispatch spec 4.2). A held change of Paykit's member is never signed, a
    /// withdrawal included: it is cleared, and the host is told which public endpoints it
    /// would have withdrawn, offered, or given a new payload, from what the user's call asked
    /// ([`EndpointChange`]; [`crate::NoticeKind::PublicEndpointChangeNotPublished`]). The
    /// notice is raised before the clear, so a crash between them tells it again; a notice
    /// that can't be raised for a reason other than the store's doesn't keep the held
    /// change, which would hold the record's publication. Held accepted kinds are worked out
    /// again from every protocol's registration. The client's `RecordEditsHeld` duty for
    /// Paykit is then acknowledged.
    ///
    /// # Errors
    ///
    /// A store failure or an alarm.
    pub(crate) async fn settle_held_record_edits(&mut self) -> Result<()> {
        if self.own_record_unknown || self.require_first_read().is_err() {
            return Ok(());
        }
        match self.app.held_accepts().await {
            Ok(None) => {}
            Ok(Some(_)) | Err(AppError::InvalidData(_)) => {
                self.app.clear_held_accepts().await?;
                if let Err(error @ (AppError::Store(_) | AppError::Alarm(_))) =
                    self.app.update_accepts().await
                {
                    return Err(error.into());
                }
            }
            Err(error) => return Err(error.into()),
        }
        // The held member's endpoints, if they differ from the record signed now.
        let endpoints_differ = match self.app.held_record_edit(PROTOCOL_NAME).await {
            Ok(None) => None,
            Ok(Some(held)) => Some(match PaykitMember::from_member(held.member.as_ref()) {
                Ok(member) => member.public_endpoints != self.own_member()?.public_endpoints,
                Err(_) => true,
            }),
            // An entry that doesn't read may hold a change of the endpoints.
            Err(AppError::InvalidData(_)) => Some(true),
            Err(error) => return Err(error.into()),
        };
        if let Some(endpoints_differ) = endpoints_differ {
            // What the user's call asked, when its record reads. A held change of only the
            // capabilities, deadline or renewal, which no call of the user's asked, is worked
            // out again by the upkeep and told to no one.
            let asked = self
                .local_get::<EndpointChange>(key::ENDPOINT_CHANGE)
                .await
                .unwrap_or_default();
            if asked.is_some() || endpoints_differ {
                if let Err(error @ PaykitError::Client(AppError::Store(_) | AppError::Alarm(_))) =
                    self.endpoint_change_not_published(asked.unwrap_or_default())
                        .await
                {
                    return Err(error);
                }
            }
            self.app.clear_held_record_edit(PROTOCOL_NAME).await?;
            self.local_delete(key::ENDPOINT_CHANGE).await?;
        } else if !self.desired_record().await?.pending {
            // Nothing held and nothing of this run left unsigned: the change was signed.
            self.local_delete(key::ENDPOINT_CHANGE).await?;
        }
        self.ack_record_edits_held().await
    }

    /// Acknowledge for Paykit the client's `RecordEditsHeld` duties, once nothing is held for
    /// it ([`Paykit::settle_held_record_edits`]).
    async fn ack_record_edits_held(&mut self) -> Result<()> {
        let ids: Vec<_> = self
            .app
            .pending_duty_events()
            .await?
            .into_iter()
            .filter(|duty| {
                matches!(
                    duty.event,
                    pubky_dispatch_client::app::AppEvent::RecordEditsHeld { .. }
                ) && duty.owners.iter().any(|owner| owner == PROTOCOL_NAME)
            })
            .filter_map(|duty| duty.event.id())
            .collect();
        for id in ids {
            self.ack_taken(id).await?;
        }
        Ok(())
    }

    /// The app's standing the steps after the upkeep act under ([`OwnStanding`]).
    fn own_standing(&self) -> OwnStanding {
        OwnStanding {
            cert: self.app.cert().hash(),
            identity: self.app.own_identity().hash(),
            recovery: self.app.recovery_required().map(|notice| notice.generation),
        }
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
        let publication = result?;
        // Signed: no change of this run is left to tell.
        self.local_delete(key::ENDPOINT_CHANGE).await?;
        Ok(publication)
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
            return Err(Refusal::HeldForUser(crate::HeldForUserReason::ReloadRequired).into());
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

/// The public endpoints the user's last call would withdraw, offer, or give a new payload, by
/// Payment Endpoint Identifier, against the record signed then (Paykit spec 4, "Changes saved
/// and not yet signed"). Kept before the client is asked to save the change, only to tell the
/// host exactly what a held change was; never read to sign. Gone once the change is signed,
/// told, or replaced by a later call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EndpointChange {
    pub(crate) withdrawn: BTreeSet<String>,
    pub(crate) offered: BTreeSet<String>,
    pub(crate) changed: BTreeSet<String>,
}

impl EndpointChange {
    pub(crate) fn is_empty(&self) -> bool {
        self.withdrawn.is_empty() && self.offered.is_empty() && self.changed.is_empty()
    }
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
/// record's null deadline before joining. Later changes are checked by the Dispatch
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
    let refused = |error: pubky_dispatch_core::Error| {
        PaykitError::from(AppError::from(pubky_dispatch_client::app::Refusal::Usage(
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

/// The reason [`Paykit::tend_own_record`] gives a record expiry policy this version doesn't
/// know.
const UNKNOWN_EXPIRY_POLICY: &str = "unknown Dispatch app-record expiry policy";

/// What the steps of a receive after the app record's upkeep act under: the app's
/// certificate, the newest own `identity.json` (its generation, version and listed apps among
/// it) and the recovery notice held, if any (Dispatch spec 3.6, 12.4).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OwnStanding {
    cert: [u8; 32],
    identity: [u8; 32],
    recovery: Option<u64>,
}

/// The outcome of a receive's upkeep that failed with `error`, the app's standing `before` and
/// `after` it: the reason the record is owed when the failure is the publisher's own
/// ([`own_record_left_owed`]) and the standing is unchanged, else `error`, which ends the
/// receive.
pub(crate) fn upkeep_outcome(
    error: PaykitError,
    before: &OwnStanding,
    after: &OwnStanding,
) -> Result<Option<String>> {
    if before != after {
        return Err(error);
    }
    own_record_left_owed(&error)
        .map(|reason| Some(reason.to_string()))
        .ok_or(error)
}

/// What the app record's upkeep did ([`Paykit::tend_own_record`]).
pub(crate) enum Tended {
    /// Nothing tried: the store couldn't tell whether the last write of the record was
    /// applied, so the record held may not be the one stored.
    Held,
    /// The record was signed anew, changed or renewed: where it stands on the Homeserver.
    Signed(Publication),
    /// The record held went to the client unchanged: where its copy stands on the Homeserver.
    Unchanged(Publication),
}

/// Why the record is owed when the Dispatch holds it and its Homeserver write failed
/// ([`Paykit::tend_own_record_at_receive`]). Shown to hosts as it is.
/// Why the app record's public copy on the Homeserver is owed, where the record stands as
/// `publication` after the upkeep (Dispatch spec 4.2); `None` where no copy is owed. Shown to
/// hosts as it is.
fn publication_owed(publication: &Publication) -> Option<&'static str> {
    match publication {
        // The Dispatch holds the record and its Homeserver write failed: the client writes it
        // again at a later read.
        Publication::Owed { .. } => Some(
            "the app record's copy on the Homeserver couldn't be written; it is written again \
             at a later receive",
        ),
        // The copy is missing and the app shares its keys with other devices, or a restore's
        // first read hasn't happened yet: nothing is written until a read shows the app
        // current (Dispatch spec 4.2).
        Publication::AwaitingRead => Some(
            "the app record's copy on the Homeserver waits for a read: a missing copy is \
             written back once the dispatch shows the app current, and after a restore nothing \
             is written before the first receive",
        ),
        // The Dispatch has accepted no record of the app yet, as after a provider loss or a
        // restore from a backup that carries none: nothing is published until it does.
        Publication::NothingAccepted => Some(
            "the dispatch has accepted no app record yet; its copy on the Homeserver is \
             written once it does",
        ),
        // A newer record choice of the app's own is unresolved, and the accepted copy is
        // repaired only once it settles.
        Publication::AwaitingRecord => Some(
            "the app record's copy on the Homeserver waits for a newer change of the record to \
             settle",
        ),
        // The app doesn't publish its record: no copy is ever owed.
        Publication::NotPublishing
        // The Homeserver holds the newest record the Dispatch accepted.
        | Publication::Published
        // The Homeserver holds a later record of the app than the Dispatch accepted, which
        // nothing writes over.
        | Publication::Superseded
        // Nothing is owed as far as the client knows; only its check of the copy failed,
        // and it checks again at the next read.
        | Publication::Unchecked { .. } => None,
        // A state this version doesn't know: told, so the host doesn't take the copy as
        // published.
        _ => Some("the app record's copy on the Homeserver is in a state this version doesn't know"),
    }
}

/// Why the record is owed while the store can't tell whether the last write of it was
/// applied ([`Paykit::tend_own_record_at_receive`]). Shown to hosts as it is.
pub(crate) const OWED_UNTIL_LOADED: &str =
    "the app record isn't updated until the app is loaded again: \
     the store couldn't tell whether its last write of the record was applied";

/// Why a failure of the app record's upkeep leaves the record owed without ending a receive
/// ([`Paykit::tend_own_record_at_receive`]); `None` for any other failure, which ends it, as
/// every failure there did before.
///
/// An allow-list, so the receive goes on past an upkeep failure only when every later step
/// would take the same failure as one to try again later:
/// - the Dispatch client's `Usage` refusal of the update itself, a candidate or an `app.put` it
///   can't sign, a list of what the payment method pays that the record can't carry among
///   them (Dispatch wire 6.1), and the edit the client keeps pending for it: refused before
///   anything reached the Dispatch, it says nothing of the Dispatch's answer to the later steps;
/// - an expiry policy this version doesn't know;
/// - an own Dispatch that doesn't answer now, by the very rule the later steps use to tell a
///   write to try again from one refused ([`crate::pay::dispatch_unavailable`]).
///
/// Everything else ends the receive: a wallet-wide stop, any other refusal or answer of the
/// Dispatch, the store, an alarm, a held record or member that doesn't read.
///
/// Security: the reason is fixed text, shown to hosts as it is. It never carries the error's
/// own text, which can hold a service's answer; an update of the record made on its own
/// returns the error itself.
pub(crate) fn own_record_left_owed(error: &PaykitError) -> Option<&'static str> {
    let client = match error {
        PaykitError::Client(client) | PaykitError::PublicEndpointChangePending(client) => client,
        PaykitError::Refused(Refusal::Usage(reason)) if reason == UNKNOWN_EXPIRY_POLICY => {
            return Some(
                "the app record has an expiry policy this version doesn't know; a newer \
                 version brings it up to date",
            );
        }
        _ => return None,
    };
    match client.unkept() {
        AppError::Refused(ClientRefusal::Usage(_)) => Some(
            "the app record's update was refused as one the record can't carry; it is tried \
             again at the next receive",
        ),
        _ if crate::pay::dispatch_unavailable(client) => Some(
            "the own dispatch didn't answer for the app record's update; it is tried again at \
             the next receive",
        ),
        _ => None,
    }
}
#[cfg(test)]
mod renewal_tests {
    use pubky_dispatch_client::app::Protocol;
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

#[cfg(test)]
mod upkeep_tests {
    use pubky_dispatch_client::error::{ClientError, TransportError};
    use pubky_dispatch_core::http::{not_hosted_current, ErrorCode};

    use super::*;
    use crate::pay::dispatch_unavailable;

    fn owed(error: AppError) -> bool {
        own_record_left_owed(&PaykitError::Client(error)).is_some()
    }

    /// The Dispatch's answer `code`, with `current`.
    fn answered(code: ErrorCode, current: serde_json::Value) -> AppError {
        AppError::Dispatch(ClientError::Dispatch {
            status: code.status(),
            code,
            message: String::new(),
            current,
            known_version: None,
            retry_after: None,
        })
    }

    fn kept(error: AppError) -> AppError {
        AppError::Kept {
            batch_id: [7; 16],
            error: Box::new(error),
        }
    }

    fn standing(cert: u8, identity: u8, recovery: Option<u64>) -> OwnStanding {
        OwnStanding {
            cert: [cert; 32],
            identity: [identity; 32],
            recovery,
        }
    }

    /// The gate's invariant over every answer of the dispatch: the receive goes on past an
    /// upkeep failure only when a later step would try the same failure again later
    /// ([`dispatch_unavailable`]), kept or not, and under a pending edit too.
    #[test]
    fn test_own_record_left_owed_only_where_later_steps_try_again() {
        for code in ErrorCode::ALL {
            // No wildcard: a code the Dispatch adds fails to build here until it is classified.
            let try_again = match code {
                ErrorCode::Unavailable | ErrorCode::Staged => true,
                ErrorCode::BadRequest
                | ErrorCode::BadSignature
                | ErrorCode::StaleTimestamp
                | ErrorCode::NotAdmitted
                | ErrorCode::PermissionMissing
                | ErrorCode::NotCurrent
                | ErrorCode::CertExpired
                | ErrorCode::NotFound
                | ErrorCode::Conflict
                | ErrorCode::StaleEpoch
                | ErrorCode::StaleGeneration
                | ErrorCode::StaleKey
                | ErrorCode::StaleIdentity
                | ErrorCode::IdentityNeeded
                | ErrorCode::Moved
                | ErrorCode::TooLarge
                | ErrorCode::RateLimited
                | ErrorCode::QuotaExceeded
                | ErrorCode::RecoveryRequired => false,
            };
            for current in [serde_json::Value::Null, not_hosted_current()] {
                let answer = answered(code, current);
                assert_eq!(dispatch_unavailable(&answer), try_again, "{code:?}");
                assert_eq!(owed(answer.clone()), try_again, "{code:?}");
                assert_eq!(owed(kept(answer.clone())), try_again, "{code:?} kept");
                assert_eq!(
                    own_record_left_owed(&PaykitError::PublicEndpointChangePending(answer))
                        .is_some(),
                    try_again,
                    "{code:?} pending"
                );
            }
        }
    }

    /// The invariant over the client's other failures of a call to the dispatch.
    #[test]
    fn test_own_record_left_owed_follows_the_later_steps_for_every_client_failure() {
        for (error, try_again) in [
            (ClientError::Transport(TransportError::Timeout), true),
            (ClientError::BadServiceSignature { status: 503 }, true),
            (ClientError::BadServiceSignature { status: 401 }, false),
            (ClientError::OwnRecoveryRequired { generation: 1 }, false),
            (ClientError::DisplayReadPending, false),
            (ClientError::StaleAnswer { server_time_ms: 1 }, false),
            (
                ClientError::BodyUnread {
                    status: 429,
                    code: ErrorCode::RateLimited,
                    retry_after: None,
                },
                false,
            ),
            (ClientError::BodyOverCap { cap: 1 }, false),
            (ClientError::Unpinned, false),
            (ClientError::InvalidResponse("member".into()), false),
            (ClientError::Usage("call".into()), false),
            (
                ClientError::Chain(pubky_dispatch_core::chain::ChainError::Malformed(
                    "stub".into(),
                )),
                false,
            ),
            (
                ClientError::Core(pubky_dispatch_core::Error::Invalid("document".into())),
                false,
            ),
        ] {
            let error = AppError::Dispatch(error);
            assert_eq!(dispatch_unavailable(&error), try_again, "{error:?}");
            assert_eq!(owed(error.clone()), try_again, "{error:?}");
            assert_eq!(owed(kept(error.clone())), try_again, "{error:?} kept");
        }
        for (error, try_again) in [
            (AppError::Transport(TransportError::Timeout), true),
            (AppError::NotAnsweringAsAccount { since_ms: 1 }, true),
            (AppError::Store(StoreError::Unknown), false),
            (AppError::InvalidData("kept batch".into()), false),
        ] {
            assert_eq!(dispatch_unavailable(&error), try_again, "{error:?}");
            assert_eq!(owed(error.clone()), try_again, "{error:?}");
        }
    }

    #[test]
    fn test_own_record_left_owed_takes_only_the_clients_usage_refusal() {
        let usage = || AppError::Refused(ClientRefusal::Usage("too large".into()));
        assert!(owed(usage()));
        // Kept, as the client keeps a batch it couldn't settle.
        assert!(owed(kept(usage())));
        // An edit the client keeps pending for the refusal is owed as the refusal is.
        assert!(own_record_left_owed(&PaykitError::PublicEndpointChangePending(usage())).is_some());
        for refusal in [
            ClientRefusal::Stopped,
            // The client took a later generation's identity.json: the wallet's view moved.
            ClientRefusal::NotNow("a later generation".into()),
            ClientRefusal::NotJoined,
            ClientRefusal::RecoveryRequired { generation: 1 },
        ] {
            assert!(!owed(AppError::Refused(refusal.clone())), "{refusal:?}");
        }
    }

    #[test]
    fn test_own_record_left_owed_takes_only_the_unknown_expiry_policy() {
        assert!(
            own_record_left_owed(&Refusal::Usage(UNKNOWN_EXPIRY_POLICY.into()).into()).is_some()
        );
        for refused in [
            Refusal::Usage("load the app again".into()),
            Refusal::NotNow(crate::NotNowReason::CallAgain("load the app again")),
            Refusal::IdentityUnavailable("first read".into()),
        ] {
            assert_eq!(
                own_record_left_owed(&refused.clone().into()),
                None,
                "{refused:?}"
            );
        }
        assert_eq!(own_record_left_owed(&PaykitError::corrupt()), None);
    }

    #[test]
    fn test_own_record_publication_owed_for_every_state_that_leaves_the_copy_owed() {
        let failed = || TransportError::Timeout;
        for (publication, owed) in [
            (Publication::Owed { error: failed() }, true),
            (Publication::AwaitingRead, true),
            (Publication::NothingAccepted, true),
            (Publication::AwaitingRecord, true),
            (Publication::NotPublishing, false),
            (Publication::Published, false),
            (Publication::Superseded, false),
            (Publication::Unchecked { error: failed() }, false),
        ] {
            assert_eq!(
                publication_owed(&publication).is_some(),
                owed,
                "{publication:?}"
            );
        }
    }

    #[test]
    fn test_own_record_upkeep_outcome_ends_the_receive_once_the_standing_moved() {
        let owed = || PaykitError::Client(AppError::Transport(TransportError::Timeout));
        let before = standing(1, 1, None);
        assert!(upkeep_outcome(owed(), &before, &standing(1, 1, None))
            .unwrap()
            .is_some());
        // A renewal taken, a newer identity.json, a recovery notice: the failure ends the
        // receive, whatever it is.
        for after in [
            standing(2, 1, None),
            standing(1, 2, None),
            standing(1, 1, Some(2)),
        ] {
            assert!(
                upkeep_outcome(owed(), &before, &after).is_err(),
                "{after:?}"
            );
        }
    }
}
