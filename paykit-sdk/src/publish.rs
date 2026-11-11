//! Publishing Private Payment Lists, the payee's side (Paykit spec 8.4 "Publishing", Paykit
//! wire 4.13, 4.14).
//!
//! The app keeps its list ([`key::LIST`]): each endpoint reusable, with its payload, or
//! single-use, its payload issued fresh by the payment method for each counterparty and each
//! version ([`crate::Rail::issue_single_use`]). It sends a counterparty a newer version:
//!
//! - when the host chooses it ([`Paykit::publish_list_to`]), for example for each new contact;
//! - when the list changes ([`Paykit::set_list_endpoints`]), to every counterparty that gets
//!   the app's lists;
//! - when a proof names a single-use endpoint of the version it holds;
//! - when its `private_payment_list_request` names a used version, an expired one, or none
//!   (`held_version: null`);
//! - at a receive once less than half of its current version's lifetime remains (Renewal).
//!
//! Each version is the later of the device clock and one past the last version published
//! ([`key::LAST_VERSION`]), so it is above every earlier one, also after a restore; its
//! `expires_at` is [`LIST_LIFETIME_MS`] after the device clock.
//!
//! Per counterparty the app keeps what it sent (`lp/<counterparty>`): each version still
//! tracked, its expiry, when the counterparty's inbox accepted it, the payloads it offered and
//! whether a proof used it. The payee's check of a proof reads the payload it offered
//! ([`Paykit::verify_payment`]). A version is tracked until [`KEPT_MESSAGE_MS`] after its expiry,
//! the time a payer keeps a proof to send again (Paykit spec 9).
//!
//! **Releases.** A single-use payload goes back to the payment method
//! ([`crate::Rail::release_single_use`]) only once no payer can pay it to this app:
//!
//! - its version wasn't sent: issuing another payload, signing or recording it failed;
//! - its version never reached the counterparty: the delivery failed for good, or the message
//!   left the outbox unaccepted, or a newer version reached the counterparty's inbox first,
//!   so the payer ignores it (Paykit wire 4.13). That a message the outbox no longer keeps
//!   was given up undelivered, the client tells by a duty event, which the receive lists
//!   once after its read ([`Paykit::receive`]): only a receive decides such a version;
//! - this app was removed from its identity: payers ignore a removed app's destinations
//!   (Paykit spec 4).
//!
//! A version the counterparty's inbox accepted while it was the newest stays the method's to
//! expire, as v0.1 left a shared reservation: a payment the payer started before a newer
//! version arrived still goes on. A release is kept (`rl/<counterparty>/<version>`) until the
//! method takes it, and asked again at each receive.

use std::collections::BTreeMap;

use paykit_lib::bodies::{ListEndpoint, PrivatePaymentList, PrivatePaymentListRequest};
use paykit_lib::{Body, Kind, PaymentEndpointIdentifier, PaymentEndpointPayload};
use pubky_switch_client::app::{AppEvent, Delivery, ReceiveContext, SENT_RETENTION_MS};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::records::DAY_MS;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::rail::{SingleUseIssue, SingleUseRelease};
use crate::receive::{Fail, KEPT_MESSAGE_MS};
use crate::state::{self, key, StateKey, Tx};

/// How long a Private Payment List this app publishes is usable: 30 days from publishing, as
/// the prototype sets (Paykit wire 4.13), the most a payer uses one for anyway.
pub const LIST_LIFETIME_MS: u64 = 30 * DAY_MS;

/// One endpoint of the app's Private Payment List ([`Paykit::set_list_endpoints`]).
///
/// A reusable payload is sensitive, as every payload is: `Debug` shows only the kind.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OfferedEndpoint {
    /// A reusable endpoint, the same payload for every counterparty and version.
    Reusable(PaymentEndpointPayload),
    /// A single-use endpoint: its payload is issued by the payment method for each
    /// counterparty and each version ([`crate::Rail::issue_single_use`]), and paying it
    /// consumes that version (Paykit spec 8.4).
    SingleUse,
}

impl std::fmt::Debug for OfferedEndpoint {
    // The payload stays out of logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Reusable(_) => "Reusable(..)",
            Self::SingleUse => "SingleUse",
        })
    }
}

/// A list version this app sent a counterparty.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SentList {
    /// The counterparty.
    pub counterparty: [u8; 32],
    /// The list's version.
    pub version: u64,
    /// The message's `msg_id`, for [`Paykit::message_delivery`].
    pub msg_id: String,
}

/// A list this app didn't send a counterparty.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListNotSent {
    /// The counterparty.
    pub counterparty: [u8; 32],
    /// Why, without content.
    pub reason: String,
}

/// What a call that publishes lists sent, and what it didn't.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListsPublished {
    /// The versions sent.
    pub sent: Vec<SentList>,
    /// The counterparties a version wasn't sent to. A version owed is tried again at the next
    /// receive.
    pub not_sent: Vec<ListNotSent>,
}

/// A counterparty this app sends its lists to, or did ([`Paykit::published_lists`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PublishedList {
    /// The counterparty.
    pub counterparty: [u8; 32],
    /// Whether it still gets the app's lists: `false` once withdrawn.
    pub active: bool,
    /// The newest version sent.
    pub version: u64,
    /// That version's `expires_at`, in Unix milliseconds.
    pub expires_at_ms: u64,
    /// That version's `msg_id`.
    pub msg_id: String,
}

/// The app's list as kept (`ll`): each identifier's reusable payload, or `None` for a
/// single-use endpoint.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeptList {
    pub(crate) endpoints: BTreeMap<String, Option<String>>,
}

impl KeptList {
    /// Every identifier and payload as the wire spells them (Paykit wire 1.4).
    pub(crate) fn check(&self) -> Result<()> {
        for (id, payload) in &self.endpoints {
            PaymentEndpointIdentifier::parse(id)?;
            if let Some(payload) = payload {
                PaymentEndpointPayload::parse(payload)?;
            }
        }
        Ok(())
    }
}

/// Where a version sent stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Fate {
    /// Not known yet.
    Pending,
    /// The counterparty's inbox accepted it while it was the newest: the payer may pay it.
    Reached,
    /// No payer can pay it: its single-use payloads were given back.
    Unreached,
}

/// An endpoint a version offered.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OfferedPayload {
    pub(crate) payload: String,
    pub(crate) single_use: bool,
}

/// A version sent to one counterparty.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Offered {
    pub(crate) version: u64,
    pub(crate) published_at_ms: u64,
    pub(crate) expires_at_ms: u64,
    pub(crate) msg_id: String,
    /// When the counterparty's inbox accepted it, once known.
    pub(crate) accepted_at_ms: Option<u64>,
    pub(crate) fate: Fate,
    pub(crate) endpoints: BTreeMap<String, OfferedPayload>,
    /// A proof named one of its single-use endpoints: the version is consumed.
    pub(crate) used: bool,
}

impl Offered {
    /// Its single-use payloads, by identifier.
    fn single_use(&self) -> BTreeMap<String, String> {
        self.endpoints
            .iter()
            .filter(|(_, e)| e.single_use)
            .map(|(id, e)| (id.clone(), e.payload.clone()))
            .collect()
    }
}

/// What this app sent one counterparty (`lp/<counterparty>`), oldest version first; the
/// counterparty repeats the key's, so a restore binds the value to its key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Recipient {
    #[serde(with = "serde_b64")]
    pub(crate) counterparty: [u8; 32],
    /// It still gets the app's lists.
    pub(crate) active: bool,
    /// A newer version is owed, set by the receive step.
    pub(crate) owed: bool,
    pub(crate) versions: Vec<Offered>,
}

impl Recipient {
    fn current(&self) -> Option<&Offered> {
        self.versions.last()
    }
}

/// Single-use payloads to give back (`rl/<counterparty>/<version>`), by identifier; the
/// counterparty and version repeat the key's.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Releases {
    #[serde(with = "serde_b64")]
    pub(crate) counterparty: [u8; 32],
    pub(crate) version: u64,
    pub(crate) payloads: BTreeMap<String, String>,
}

/// Whether `value`, kept at a key of the publishing state's `family` in a backup, reads as
/// its family's and is bound to what its key names. It is the app's own data, so the check
/// guards against corruption only.
pub(crate) fn check_restored(family: &StateKey, value: &[u8]) -> bool {
    match family {
        StateKey::List => state::decode::<KeptList>(value).is_ok_and(|list| list.check().is_ok()),
        StateKey::LastVersion => state::decode::<u64>(value).is_ok(),
        StateKey::ListRecipient(counterparty) => state::decode::<Recipient>(value)
            .is_ok_and(|recipient| recipient.counterparty == *counterparty),
        StateKey::Releases {
            counterparty,
            version,
        } => state::decode::<Releases>(value).is_ok_and(|releases| {
            releases.counterparty == *counterparty && releases.version == *version
        }),
        _ => false,
    }
}

/// The version after `last`, at `now`: above every earlier one (Paykit wire 4.13).
pub(crate) fn next_version(now: u64, last: Option<u64>) -> u64 {
    last.map_or(now, |last| now.max(last.saturating_add(1)))
}

/// Whether less than half of `offered`'s lifetime remains at `now` (Paykit wire 4.13,
/// Renewal).
pub(crate) fn renewal_due(offered: &Offered, now: u64) -> bool {
    let lifetime = offered
        .expires_at_ms
        .saturating_sub(offered.published_at_ms);
    now >= offered.published_at_ms.saturating_add(lifetime / 2)
}

/// Keep `releases` to give back, in `tx`; nothing for one without payloads.
fn release_in(tx: &mut Tx, releases: &Releases) -> Result<()> {
    if !releases.payloads.is_empty() {
        tx.put(
            &key::releases(&releases.counterparty, releases.version),
            releases,
        )?;
    }
    Ok(())
}

fn read_recipient(
    cx: &ReceiveContext<'_>,
    sender: &[u8; 32],
) -> std::result::Result<Option<Recipient>, Fail> {
    match cx.local_get(key::list_recipient(sender).as_bytes())? {
        Some(bytes) => state::decode(&bytes).map(Some).map_err(|_| Fail::corrupt()),
        None => Ok(None),
    }
}

fn keep_recipient(
    cx: &mut ReceiveContext<'_>,
    recipient: &Recipient,
) -> std::result::Result<(), Fail> {
    let write = state::local_put(&key::list_recipient(&recipient.counterparty), recipient)
        .map_err(|_| Fail::corrupt())?;
    crate::receive::keep_local(cx, write);
    Ok(())
}

/// The receive step's part for a proof naming a single-use endpoint of a version this app
/// sent its sender (Paykit spec 8.4): the version is consumed and, when it is the newest, a
/// newer one is owed. A proof naming another app, a version or endpoint this app didn't offer
/// that sender, or a reusable endpoint changes nothing; nor does a second proof of one
/// version.
pub(crate) fn note_proof(
    cx: &mut ReceiveContext<'_>,
    stored: &paykit_lib::records::StoredMessage,
) -> std::result::Result<(), Fail> {
    let Body::PaymentProof(proof) = stored.body() else {
        return Ok(());
    };
    if proof.payment_app_id != cx.app_id() {
        return Ok(());
    }
    let Some(mut recipient) = read_recipient(cx, &stored.sender())? else {
        return Ok(());
    };
    let newest = recipient.current().map(|o| o.version) == proof.list_version;
    let Some(offered) = recipient
        .versions
        .iter_mut()
        .find(|o| Some(o.version) == proof.list_version)
    else {
        return Ok(());
    };
    let single_use = offered
        .endpoints
        .get(proof.payment_endpoint_identifier.as_str())
        .is_some_and(|e| e.single_use);
    if !single_use {
        return Ok(());
    }
    // A second proof of one version names one no longer the newest once a newer one was
    // sent. The newest version of a counterparty withdrawn from is empty: no proof names it.
    offered.used = true;
    recipient.owed |= newest;
    keep_recipient(cx, &recipient)
}

/// The receive step's part for a `private_payment_list_request` (Paykit spec 8.4, Paykit wire
/// 4.14): from a counterparty that gets this app's lists, about this app or none, naming a
/// version used, expired at the request's acceptance, or none, a newer version is owed. A
/// version this app no longer tracks expired, or was lost by a restore: it is answered too.
/// Any other request, such as one from a counterparty the app never sent a list, or naming
/// the version the app holds as usable, changes nothing.
pub(crate) fn note_list_request(
    cx: &mut ReceiveContext<'_>,
    stored: &paykit_lib::records::StoredMessage,
    request: &PrivatePaymentListRequest,
) -> std::result::Result<(), Fail> {
    if request.app_id.is_some_and(|app| app != cx.app_id()) {
        return Ok(());
    }
    // A counterparty withdrawn from may owe a list here: none is sent to it.
    let Some(mut recipient) = read_recipient(cx, &stored.sender())? else {
        return Ok(());
    };
    let at = stored.accepted_at().unwrap_or(0);
    let unusable = request.held_version.is_none_or(|held| {
        recipient
            .versions
            .iter()
            .find(|o| o.version == held)
            .is_none_or(|o| o.used || o.expires_at_ms <= at)
    });
    if unusable {
        recipient.owed = true;
        keep_recipient(cx, &recipient)?;
    }
    Ok(())
}

/// The `msg_id`s of messages the client gave up delivering, never accepted
/// (`AppEvent::MessageUndelivered`), among a receive's duty events.
fn undelivered(duties: &[AppEvent]) -> Vec<String> {
    duties
        .iter()
        .filter_map(|event| match event {
            AppEvent::MessageUndelivered {
                msg_id,
                resend: false,
                ..
            } => Some(msg_id.clone()),
            _ => None,
        })
        .collect()
}

impl Paykit {
    /// Set the app's Private Payment List to `endpoints` (Paykit spec 8.4): each one
    /// reusable, with its payload, or single-use, its payload issued for each counterparty
    /// and each version ([`crate::Rail::issue_single_use`]). When it changes, every
    /// counterparty that gets the app's lists is sent a newer version; the same list again
    /// sends nothing. An empty list is allowed: it offers nothing.
    ///
    /// The list must fit one message (Switch wire 5.1: one envelope's JSON, with the
    /// message's other members): one whose reusable endpoints alone don't fit is refused,
    /// with nothing kept. The payloads issued for a counterparty may still take a version
    /// past it: that version isn't sent, its payloads are released, and the counterparty is
    /// in [`ListsPublished::not_sent`].
    ///
    /// # Errors
    ///
    /// [`Refusal::CertificateRefused`]; [`Refusal::ScopeMissing`] without `request` or a
    /// label; [`PaykitError::Lib`] `Validation` for a list that doesn't fit; a store failure.
    pub async fn set_list_endpoints(
        &mut self,
        endpoints: BTreeMap<PaymentEndpointIdentifier, OfferedEndpoint>,
    ) -> Result<ListsPublished> {
        self.refresh_own()?;
        self.require_payee()?;
        let list = KeptList {
            endpoints: endpoints
                .iter()
                .map(|(id, endpoint)| {
                    let payload = match endpoint {
                        OfferedEndpoint::Reusable(payload) => Some(payload.as_str().to_owned()),
                        OfferedEndpoint::SingleUse => None,
                    };
                    (id.as_str().to_owned(), payload)
                })
                .collect(),
        };
        let fits = PrivatePaymentList {
            version: next_version(self.now(), self.local_get(key::LAST_VERSION).await?),
            payment_endpoints: endpoints
                .iter()
                .filter_map(|(id, endpoint)| match endpoint {
                    OfferedEndpoint::Reusable(payload) => Some((
                        id.clone(),
                        ListEndpoint {
                            payload: payload.clone(),
                            single_use: false,
                        },
                    )),
                    OfferedEndpoint::SingleUse => None,
                })
                .collect(),
            expires_at: self.now().saturating_add(LIST_LIFETIME_MS),
        };
        self.signed_list(self.identity(), &fits)?;
        if self.local_get::<KeptList>(key::LIST).await?.as_ref() == Some(&list) {
            return Ok(ListsPublished::default());
        }
        self.local_put(key::LIST, &list).await?;
        let mut active = Vec::new();
        for (_, recipient) in self.local_scan::<Recipient>(key::LIST_RECIPIENTS).await? {
            if recipient.active {
                active.push(recipient.counterparty);
            }
        }
        self.publish_to(&active, false, None).await
    }

    /// Send each of `counterparties` a newer version of the app's list (Paykit spec 8.4), for
    /// example each new contact: from then on each gets the app's lists, renewals and the
    /// answers above included, until [`Paykit::withdraw_list_from`]. A version not sent is in
    /// [`ListsPublished::not_sent`], its payloads released.
    ///
    /// # Errors
    ///
    /// [`Refusal::CertificateRefused`]; [`Refusal::ScopeMissing`] without `request` or a
    /// label; [`Refusal::Usage`] before any list is set, or for the app's own identity; a
    /// store failure.
    pub async fn publish_list_to(&mut self, counterparties: &[[u8; 32]]) -> Result<ListsPublished> {
        self.refresh_own()?;
        self.require_payee()?;
        if self.local_get::<KeptList>(key::LIST).await?.is_none() {
            return Err(Refusal::Usage("Paykit spec 8.4: no list is set".into()).into());
        }
        if counterparties.contains(&self.identity()) {
            return Err(
                Refusal::Usage("Paykit spec 8.4: a list goes to another identity".into()).into(),
            );
        }
        self.publish_to(counterparties, false, None).await
    }

    /// Stop sending `counterparty` the app's lists, as when it is no longer a contact: it is
    /// sent a newer, empty version, which replaces the one it holds once its inbox accepts
    /// it, and gets no renewal or answer after. A version it held may still be paid until it
    /// expires, as a payment started before the empty one arrived goes on: its single-use
    /// payloads stay the payment method's to expire. [`Paykit::publish_list_to`] sends it the
    /// list again.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`] for a counterparty that never got a list; as
    /// [`Paykit::publish_list_to`].
    pub async fn withdraw_list_from(&mut self, counterparty: &[u8; 32]) -> Result<ListsPublished> {
        self.refresh_own()?;
        self.require_payee()?;
        if self
            .local_get::<Recipient>(&key::list_recipient(counterparty))
            .await?
            .is_none()
        {
            return Err(Refusal::NotFound.into());
        }
        self.publish_to(&[*counterparty], true, None).await
    }

    /// Every counterparty this app sends its lists to, or did, with the newest version sent.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn published_lists(&self) -> Result<Vec<PublishedList>> {
        let mut out = Vec::new();
        for (_, recipient) in self.local_scan::<Recipient>(key::LIST_RECIPIENTS).await? {
            if let Some(current) = recipient.current() {
                out.push(PublishedList {
                    counterparty: recipient.counterparty,
                    active: recipient.active,
                    version: current.version,
                    expires_at_ms: current.expires_at_ms,
                    msg_id: current.msg_id.clone(),
                });
            }
        }
        Ok(out)
    }

    /// The payload this app offered `payer` for `endpoint` in its list `version`, while it
    /// tracks that version (Paykit spec 8.4).
    pub(crate) async fn offered_payload(
        &self,
        payer: &[u8; 32],
        version: u64,
        endpoint: &PaymentEndpointIdentifier,
    ) -> Result<Option<PaymentEndpointPayload>> {
        let Some(recipient) = self
            .local_get::<Recipient>(&key::list_recipient(payer))
            .await?
        else {
            return Ok(None);
        };
        let offered = recipient
            .versions
            .iter()
            .filter(|o| o.version == version)
            .find_map(|o| o.endpoints.get(endpoint.as_str()));
        offered
            .map(|o| PaymentEndpointPayload::parse(&o.payload).map_err(PaykitError::from))
            .transpose()
    }

    /// The list `list` signed for `to`, refused when it doesn't fit one message.
    fn signed_list(
        &self,
        to: [u8; 32],
        list: &PrivatePaymentList,
    ) -> Result<pubky_switch_client::app::SignedMessage> {
        let body = list.to_body()?;
        self.sign(to, Kind::PrivatePaymentList, Vec::new(), body)
            .map_err(|error| match error {
                PaykitError::Client(pubky_switch_client::app::AppError::InvalidData(_)) => {
                    paykit_lib::Error::Validation(format!(
                        "Paykit wire 4.13: a Private Payment List fits one message, at most {} \
                         bytes of JSON",
                        pubky_switch_core::message::max_json_len(1).unwrap_or(0)
                    ))
                    .into()
                }
                other => other,
            })
    }

    /// Send each of `counterparties` a newer version, empty when `withdraw`, each on its own.
    /// `undelivered` is as for [`Paykit::settled`].
    async fn publish_to(
        &mut self,
        counterparties: &[[u8; 32]],
        withdraw: bool,
        undelivered: Option<&[String]>,
    ) -> Result<ListsPublished> {
        let mut out = ListsPublished::default();
        for counterparty in counterparties {
            match self.send_list(*counterparty, withdraw, undelivered).await {
                Ok(sent) => out.sent.push(sent),
                Err(error) => out.not_sent.push(ListNotSent {
                    counterparty: *counterparty,
                    reason: error.to_string(),
                }),
            }
        }
        self.run_releases().await?;
        Ok(out)
    }

    /// Send `counterparty` a newer version of the app's list, or an empty one when
    /// `withdraw`. The version is recorded before it is queued: a crash between the two
    /// leaves a version that never left, whose payloads the next receive releases.
    async fn send_list(
        &mut self,
        counterparty: [u8; 32],
        withdraw: bool,
        undelivered: Option<&[String]>,
    ) -> Result<SentList> {
        let list = if withdraw {
            KeptList::default()
        } else {
            self.local_get::<KeptList>(key::LIST)
                .await?
                .ok_or_else(|| Refusal::Usage("Paykit spec 8.4: no list is set".into()))?
        };
        let rkey = key::list_recipient(&counterparty);
        let mut recipient = match self.local_get::<Recipient>(&rkey).await? {
            Some(held) => self.settled(held, undelivered).await?,
            None => Recipient {
                counterparty,
                active: true,
                owed: false,
                versions: Vec::new(),
            },
        };
        let now = self.now();
        let version = next_version(now, self.local_get(key::LAST_VERSION).await?);
        let expires_at_ms = now.saturating_add(LIST_LIFETIME_MS);
        let mut issued = Releases {
            counterparty,
            version,
            payloads: BTreeMap::new(),
        };
        let built = self
            .build_list(&list, &mut issued, version, expires_at_ms)
            .await
            .and_then(|built| Ok((self.signed_list(counterparty, &built)?, built)));
        let (signed, built) = match built {
            Ok(done) => done,
            Err(error) => {
                let mut tx = Tx::default();
                release_in(&mut tx, &issued)?;
                self.local_commit(tx).await?;
                return Err(error);
            }
        };
        let msg_id = signed.msg_id().to_owned();
        recipient.versions.push(Offered {
            version,
            published_at_ms: now,
            expires_at_ms,
            msg_id: msg_id.clone(),
            accepted_at_ms: None,
            fate: Fate::Pending,
            endpoints: built
                .payment_endpoints
                .iter()
                .map(|(id, e)| {
                    let offered = OfferedPayload {
                        payload: e.payload.as_str().to_owned(),
                        single_use: e.single_use,
                    };
                    (id.as_str().to_owned(), offered)
                })
                .collect(),
            used: false,
        });
        recipient.active = !withdraw;
        recipient.owed = false;
        let mut tx = Tx::default();
        tx.put(&rkey, &recipient)?
            .put(key::LAST_VERSION, &version)?;
        self.note_peer(&mut tx, &counterparty)?;
        if let Err(error) = self.local_commit(tx).await {
            let mut tx = Tx::default();
            release_in(&mut tx, &issued)?;
            let _ = self.local_commit(tx).await;
            return Err(error);
        }
        self.app.queue_signed(signed).await?;
        // Queued, the list goes at the next flush if this try fails.
        let _ = self.app.flush_message(&msg_id).await;
        Ok(SentList {
            counterparty,
            version,
            msg_id,
        })
    }

    /// The list of `kept` at `version`, its single-use payloads issued for
    /// `issued.counterparty` and noted in `issued` as they come.
    async fn build_list(
        &self,
        kept: &KeptList,
        issued: &mut Releases,
        version: u64,
        expires_at_ms: u64,
    ) -> Result<PrivatePaymentList> {
        let mut payment_endpoints = BTreeMap::new();
        for (id, payload) in &kept.endpoints {
            let endpoint_id = PaymentEndpointIdentifier::parse(id)?;
            let endpoint = match payload {
                Some(payload) => ListEndpoint {
                    payload: PaymentEndpointPayload::parse(payload)?,
                    single_use: false,
                },
                None => {
                    let payload = self
                        .rail
                        .issue_single_use(SingleUseIssue {
                            counterparty: issued.counterparty,
                            endpoint_id: endpoint_id.clone(),
                            list_version: version,
                            expires_at_ms,
                        })
                        .await?;
                    issued
                        .payloads
                        .insert(id.clone(), payload.as_str().to_owned());
                    ListEndpoint {
                        payload,
                        single_use: true,
                    }
                }
            };
            payment_endpoints.insert(endpoint_id, endpoint);
        }
        Ok(PrivatePaymentList {
            version,
            payment_endpoints,
            expires_at: expires_at_ms,
        })
    }

    /// `recipient` with each version's delivery read and its fate decided (module docs), the
    /// releases of the versions that never reached the counterparty kept, and the versions no
    /// longer tracked dropped; committed when anything changed. `undelivered` are the
    /// messages the client gave up delivering, as a receive's duty events tell
    /// ([`undelivered`]); `None` outside a receive, which lists no duty events: a version
    /// whose message the outbox no longer keeps is then left for the next receive.
    async fn settled(
        &mut self,
        mut recipient: Recipient,
        undelivered: Option<&[String]>,
    ) -> Result<Recipient> {
        let before = recipient.clone();
        let now = self.now();
        for offered in &mut recipient.versions {
            if offered.fate != Fate::Pending || offered.accepted_at_ms.is_some() {
                continue;
            }
            match self.app.delivery(&offered.msg_id).await? {
                Some(Delivery::Accepted { accepted_at_ms }) => {
                    offered.accepted_at_ms = Some(accepted_at_ms);
                }
                Some(Delivery::Failed { .. }) => offered.fate = Fate::Unreached,
                Some(_) => {}
                // Not in the outbox and never seen accepted: it never left, or the client gave
                // it up undelivered. Past half the time the client keeps a sent message, with
                // no such event, it may instead have been accepted while the app didn't run.
                None if undelivered.is_some_and(|u| u.contains(&offered.msg_id))
                    || now.saturating_sub(offered.published_at_ms) < SENT_RETENTION_MS / 2 =>
                {
                    offered.fate = Fate::Unreached;
                }
                None if undelivered.is_some() => offered.fate = Fate::Reached,
                None => {}
            }
        }
        let accepted: Vec<(u64, u64)> = recipient
            .versions
            .iter()
            .filter_map(|o| o.accepted_at_ms.map(|at| (o.version, at)))
            .collect();
        let mut tx = Tx::default();
        for offered in &mut recipient.versions {
            if offered.fate == Fate::Pending {
                if let Some(at) = offered.accepted_at_ms {
                    offered.fate = if overtaken(&accepted, offered.version, at) {
                        Fate::Unreached
                    } else {
                        Fate::Reached
                    };
                }
            }
            if offered.fate == Fate::Unreached
                && before_fate(&before, offered.version) != Fate::Unreached
            {
                release_in(
                    &mut tx,
                    &Releases {
                        counterparty: recipient.counterparty,
                        version: offered.version,
                        payloads: offered.single_use(),
                    },
                )?;
            }
        }
        // Tracked until a late proof of it can't come any more; the newest is kept while the
        // counterparty gets the app's lists.
        let tracked = |o: &Offered| now < o.expires_at_ms.saturating_add(KEPT_MESSAGE_MS);
        let newest = recipient.current().map(|o| o.version);
        let active = recipient.active;
        recipient
            .versions
            .retain(|o| tracked(o) || (active && Some(o.version) == newest));
        let rkey = key::list_recipient(&recipient.counterparty);
        let forgotten = recipient.versions.is_empty();
        if forgotten {
            tx.delete(&rkey);
        } else if recipient != before {
            tx.put(&rkey, &recipient)?;
        }
        self.local_commit(tx).await?;
        Ok(recipient)
    }

    /// The publishing steps of a receive, after the read (module docs): with `removed`, the
    /// app was removed from its identity, and every single-use payload no proof used goes
    /// back. Otherwise each counterparty's versions are settled, and a newer version is sent
    /// where one is owed or a renewal is due. The releases kept are asked of the payment
    /// method last. `duties` are the duty events the receive listed after its read, which
    /// tell the messages the client gave up ([`undelivered`]).
    pub(crate) async fn tend_lists(
        &mut self,
        removed: bool,
        duties: &[AppEvent],
    ) -> Result<ListsPublished> {
        let undelivered = undelivered(duties);
        // A certificate without a selling scope sends no list: its recipients would drop it.
        let payee = paykit_lib::scopes::acts_as_payee(self.app.cert().scopes());
        let mut owed = Vec::new();
        let now = self.now();
        let mut tx = Tx::default();
        for (_, recipient) in self.local_scan::<Recipient>(key::LIST_RECIPIENTS).await? {
            if removed {
                for offered in recipient
                    .versions
                    .iter()
                    .filter(|o| !o.used && o.fate != Fate::Unreached)
                {
                    release_in(
                        &mut tx,
                        &Releases {
                            counterparty: recipient.counterparty,
                            version: offered.version,
                            payloads: offered.single_use(),
                        },
                    )?;
                }
                tx.delete(&key::list_recipient(&recipient.counterparty));
                continue;
            }
            let recipient = self.settled(recipient, Some(&undelivered)).await?;
            let due = recipient.current().is_some_and(|o| renewal_due(o, now));
            if payee && recipient.active && (recipient.owed || due) {
                owed.push(recipient.counterparty);
            }
        }
        self.local_commit(tx).await?;
        if removed || owed.is_empty() {
            self.run_releases().await?;
            return Ok(ListsPublished::default());
        }
        self.publish_to(&owed, false, Some(&undelivered)).await
    }

    /// Ask the payment method to take back every single-use payload kept to release
    /// (`rl/`), each once per call. One it doesn't take now stays kept, for the next receive.
    pub(crate) async fn run_releases(&mut self) -> Result<()> {
        for (_, mut releases) in self.local_scan::<Releases>(key::RELEASES).await? {
            let rkey = key::releases(&releases.counterparty, releases.version);
            let mut kept = BTreeMap::new();
            for (id, payload) in std::mem::take(&mut releases.payloads) {
                let release = SingleUseRelease {
                    counterparty: releases.counterparty,
                    endpoint_id: PaymentEndpointIdentifier::parse(&id)?,
                    list_version: releases.version,
                    payload: PaymentEndpointPayload::parse(&payload)?,
                };
                if self.rail.release_single_use(release).await.is_err() {
                    kept.insert(id, payload);
                }
            }
            if kept.is_empty() {
                self.local_delete(&rkey).await?;
            } else {
                releases.payloads = kept;
                self.local_put(&rkey, &releases).await?;
            }
        }
        Ok(())
    }
}

/// Whether a version newer than `version`, accepted at `at`, was accepted before it, of the
/// `(version, accepted_at)` in `accepted`: the payer then ignores it (Paykit wire 4.13).
fn overtaken(accepted: &[(u64, u64)], version: u64, at: u64) -> bool {
    accepted.iter().any(|(v, t)| *v > version && *t < at)
}

/// The fate `version` had in `before`; `Pending` for one it didn't hold.
fn before_fate(before: &Recipient, version: u64) -> Fate {
    before
        .versions
        .iter()
        .find(|o| o.version == version)
        .map_or(Fate::Pending, |o| o.fate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered(published_at_ms: u64, expires_at_ms: u64) -> Offered {
        Offered {
            version: published_at_ms,
            published_at_ms,
            expires_at_ms,
            msg_id: "m".into(),
            accepted_at_ms: None,
            fate: Fate::Pending,
            endpoints: BTreeMap::new(),
            used: false,
        }
    }

    #[test]
    fn test_publish_version_above_the_last_and_the_clock() {
        assert_eq!(next_version(1_000, None), 1_000);
        assert_eq!(next_version(1_000, Some(400)), 1_000);
        assert_eq!(next_version(1_000, Some(1_000)), 1_001);
        assert_eq!(next_version(1_000, Some(5_000)), 5_001);
    }

    #[test]
    fn test_publish_renewal_due_once_half_the_lifetime_is_left() {
        let list = offered(1_000, 1_000 + LIST_LIFETIME_MS);
        assert!(!renewal_due(&list, 1_000 + LIST_LIFETIME_MS / 2 - 1));
        assert!(renewal_due(&list, 1_000 + LIST_LIFETIME_MS / 2));
    }

    #[test]
    fn test_publish_version_overtaken_only_by_a_newer_one_accepted_first() {
        let accepted = [(10, 500), (20, 400), (30, 900)];
        assert!(overtaken(&accepted, 10, 500));
        assert!(!overtaken(&accepted, 20, 400));
        assert!(!overtaken(&accepted, 30, 900));
        assert!(!overtaken(&[(10, 500), (20, 600)], 10, 500));
        // Accepted in one millisecond, the order isn't known: it may have reached the payer.
        assert!(!overtaken(&[(10, 500), (20, 500)], 10, 500));
    }

    #[test]
    fn test_publish_restore_binds_each_value_to_its_key() {
        let recipient = Recipient {
            counterparty: [3; 32],
            active: true,
            owed: false,
            versions: vec![offered(1, 2)],
        };
        let value = state::encode(&recipient).unwrap();
        assert!(check_restored(&StateKey::ListRecipient([3; 32]), &value));
        assert!(!check_restored(&StateKey::ListRecipient([4; 32]), &value));
        let releases = Releases {
            counterparty: [3; 32],
            version: 7,
            payloads: BTreeMap::new(),
        };
        let value = state::encode(&releases).unwrap();
        let at = |counterparty, version| StateKey::Releases {
            counterparty,
            version,
        };
        assert!(check_restored(&at([3; 32], 7), &value));
        assert!(!check_restored(&at([3; 32], 8), &value));
        assert!(!check_restored(&at([4; 32], 7), &value));
        let mut list = KeptList::default();
        list.endpoints
            .insert("btc-bitcoin-p2wpkh".into(), Some("bc1q".into()));
        assert!(check_restored(
            &StateKey::List,
            &state::encode(&list).unwrap()
        ));
        list.endpoints.insert("not an identifier".into(), None);
        assert!(!check_restored(
            &StateKey::List,
            &state::encode(&list).unwrap()
        ));
        assert!(check_restored(
            &StateKey::LastVersion,
            &state::encode(&5u64).unwrap()
        ));
        assert!(!check_restored(
            &StateKey::LastVersion,
            &state::encode(&"5").unwrap()
        ));
    }

    #[test]
    fn test_publish_offered_endpoint_debug_hides_the_payload() {
        let shown = format!(
            "{:?}",
            OfferedEndpoint::Reusable(PaymentEndpointPayload::new("lno1secret").unwrap())
        );
        assert!(!shown.contains("secret"), "{shown}");
    }
}
