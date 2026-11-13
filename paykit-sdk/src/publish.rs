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
//! - when its `private_payment_list_request` names the newest version, which counts as used
//!   by the payer's word, an expired one, or none (`held_version: null`): at most once per
//!   version named, and at most [`crate::PaykitConfig::list_answers_per_day`] newer versions
//!   in any 24 hours per counterparty in answer to requests. One naming an older version
//!   still unexpired, used or not, or a version already answered, gets the newest sent
//!   again, nothing issued, at most once an hour;
//! - at a receive once less than half of its current version's lifetime remains (Renewal).
//!
//! **Blocked counterparties** (Paykit spec 8.1; v0.1 "Block A Peer"). At an app holding
//! `contacts`, no list goes to a counterparty the deny list holds, judged as for a payment
//! the user starts ([`Paykit::deny_list`]: the copy read in the last 10 minutes, or read
//! first), nor to anyone when that read fails: no renewal, answer or re-send of a changed
//! list. What is owed it goes at the first receive after it is unblocked.
//!
//! Each version is the later of the device clock and one past the last version published
//! ([`key::LAST_VERSION`]), so it is above every earlier one, also after a restore; its
//! `expires_at` is [`LIST_LIFETIME_MS`] after the device clock.
//!
//! Per counterparty the app keeps what it sent (`lp/<counterparty>`): each version still
//! tracked, its expiry, when the counterparty's inbox accepted it, the payloads it offered and
//! whether a proof used it. A version is tracked until [`KEPT_MESSAGE_MS`] after its expiry,
//! the time a payer keeps a proof to send again (Paykit spec 9); the newest stays tracked
//! while the counterparty gets the app's lists. The payee's check of a proof reads the
//! payload it offered ([`Paykit::verify_payment`]) until that time only: a proof of a version
//! past it is checked by the payment method alone (Paykit spec 8.4, "Only what it offered").
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
//!
//! **Read back whole.** Every value of this state (`ll`, `lv`, `lp/`, `rl/`) is checked
//! whenever it is read, by the same check a restore makes, and bound to the key it is kept
//! at: a value naming another counterparty or version, or in a state no valid run produces,
//! such as what was sent a counterparty with no version left or a last version below one
//! sent, is a corrupt store, never an absence. So is a store that fails while a list is
//! sent: the call fails, rather than report the list as not sent.

use std::collections::{BTreeMap, BTreeSet};

use paykit_lib::bodies::{ListEndpoint, PrivatePaymentList, PrivatePaymentListRequest};
use paykit_lib::{Body, Kind, PaymentEndpointIdentifier, PaymentEndpointPayload};
use pubky_switch_client::app::{AppError, AppEvent, Delivery, ReceiveContext, SENT_RETENTION_MS};
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

/// How often the newest list version is sent again to one counterparty at most, in answer
/// to its list requests (Paykit spec 8.4): once an hour.
const RESEND_INTERVAL_MS: u64 = 60 * 60 * 1000;

/// The most versions a counterparty's list requests are remembered by, owed an answer
/// ([`Recipient::asking`]) or answered ([`Recipient::named`]), so its requests can't grow
/// the state without bound. A request past it is still answered: a version it named that
/// isn't remembered only gets a newer version again, not the newest sent again, within the
/// daily bound on answers.
const MAX_NAMED: usize = 16;

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
    /// The newest versions sent again, the same message, to a counterparty whose request
    /// named an older one still unexpired, or a version already answered.
    pub resent: Vec<SentList>,
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
    /// The list read whole, whenever it is read back, as a restore reads it: every
    /// identifier and payload as the wire spells them (Paykit wire 1.4). Another is a corrupt
    /// store.
    pub(crate) fn check(&self) -> Result<()> {
        for (id, payload) in &self.endpoints {
            PaymentEndpointIdentifier::parse(id).map_err(|_| PaykitError::corrupt())?;
            if let Some(payload) = payload {
                check_endpoint(id, payload)?;
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
    /// A list request named it and was answered with a newer version: a later one naming
    /// it gets the newest sent again instead.
    pub(crate) asked: bool,
}

impl Offered {
    /// The version read whole: one a run of [`Paykit::send_list`] and the steps after it
    /// can't produce is a corrupt store. Its `msg_id` is the client's, its `expires_at`
    /// [`LIST_LIFETIME_MS`] after its publishing, which is at most its version; a delivery
    /// seen accepted is decided; one marked used offered a single-use endpoint; and each
    /// endpoint is spelled as the wire spells it (Paykit wire 1.4).
    fn check(&self) -> Result<()> {
        check_msg_id(&self.msg_id)?;
        let produced = self.expires_at_ms == self.published_at_ms.saturating_add(LIST_LIFETIME_MS)
            && self.published_at_ms <= self.version
            && !(self.fate == Fate::Pending && self.accepted_at_ms.is_some())
            && (!self.used || self.endpoints.values().any(|e| e.single_use));
        if !produced {
            return Err(PaykitError::corrupt());
        }
        for (id, endpoint) in &self.endpoints {
            check_endpoint(id, &endpoint.payload)?;
        }
        Ok(())
    }

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
    /// A list request asked for a newer version: it goes within the 24-hour bound
    /// ([`crate::PaykitConfig::list_answers_per_day`]), and past it the request stays owed
    /// until the bound frees.
    pub(crate) requested: bool,
    /// The versions those requests named, marked answered once the newer version goes.
    pub(crate) asking: BTreeSet<u64>,
    /// The versions this app no longer tracks that requests named and a newer version
    /// answered, each with when, within [`KEPT_MESSAGE_MS`]: a later request naming one gets
    /// the newest sent again instead.
    pub(crate) named: Vec<(u64, u64)>,
    /// A list request named an older version still unexpired, or one already answered: the
    /// newest is sent again, at most once an hour ([`RESEND_INTERVAL_MS`]).
    pub(crate) resend: bool,
    /// When the newest version was last sent again, device clock.
    pub(crate) resent_at_ms: Option<u64>,
    /// When each newer version sent in answer to a list request went, device clock, within
    /// the last 24 hours.
    pub(crate) answers: Vec<u64>,
    pub(crate) versions: Vec<Offered>,
}

impl Recipient {
    /// A counterparty sent nothing yet.
    pub(crate) fn new(counterparty: [u8; 32]) -> Self {
        Self {
            counterparty,
            active: true,
            owed: false,
            requested: false,
            asking: BTreeSet::new(),
            named: Vec::new(),
            resend: false,
            resent_at_ms: None,
            answers: Vec::new(),
            versions: Vec::new(),
        }
    }

    fn current(&self) -> Option<&Offered> {
        self.versions.last()
    }

    /// What was sent `counterparty` read whole, at the key that names it, whenever it is
    /// read back, as a restore reads it: one naming another counterparty, or in a state no
    /// valid run produces, is a corrupt store, never an absence. A valid run keeps at least
    /// one version, oldest first, each above the one before ([`next_version`]), each read
    /// whole ([`Offered::check`]), and the versions requests named only while it owes their
    /// answer.
    pub(crate) fn check(&self, counterparty: &[u8; 32]) -> Result<()> {
        let ascending = self
            .versions
            .windows(2)
            .all(|w| w[0].version < w[1].version);
        if self.counterparty != *counterparty
            || self.versions.is_empty()
            || !ascending
            || (!self.asking.is_empty() && !self.requested)
            || self.asking.len() > MAX_NAMED
            || self.named.len() > MAX_NAMED
        {
            return Err(PaykitError::corrupt());
        }
        self.versions.iter().try_for_each(Offered::check)
    }
}

impl Releases {
    /// The payloads to give back read whole, at the key that names their counterparty and
    /// version, whenever they are read back, as a restore reads them: a valid run keeps none
    /// empty, and spells each identifier and payload as the wire does. Any other is a
    /// corrupt store.
    pub(crate) fn check(&self, counterparty: &[u8; 32], version: u64) -> Result<()> {
        if self.counterparty != *counterparty || self.version != version || self.payloads.is_empty()
        {
            return Err(PaykitError::corrupt());
        }
        self.payloads
            .iter()
            .try_for_each(|(id, payload)| check_endpoint(id, payload))
    }
}

/// A `msg_id` as the client gives one out, a lower-case UUID v4; another is a corrupt store.
pub(crate) fn check_msg_id(msg_id: &str) -> Result<()> {
    pubky_switch_core::json::check_uuid_v4(msg_id).map_err(|_| PaykitError::corrupt())
}

/// An endpoint's identifier and payload as the wire spells them (Paykit wire 1.4); another is
/// a corrupt store.
fn check_endpoint(id: &str, payload: &str) -> Result<()> {
    PaymentEndpointIdentifier::parse(id)
        .and_then(|_| PaymentEndpointPayload::parse(payload))
        .map(|_| ())
        .map_err(|_| PaykitError::corrupt())
}

/// The last version published (`lv`) against the newest version tracked for one
/// counterparty: a valid run keeps both in one commit, so `lv` is never absent or below a
/// version tracked. Checked at a restore, over every counterparty, and before each new
/// version, which [`next_version`] would otherwise not keep above it.
pub(crate) fn check_last_version(last: Option<u64>, newest: Option<u64>) -> Result<()> {
    match (last, newest) {
        (_, None) => Ok(()),
        (Some(last), Some(newest)) if newest <= last => Ok(()),
        _ => Err(PaykitError::corrupt()),
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

/// What `value`, kept at `key` of the publishing state in a backup, holds, read through
/// [`state::restored`], so bound to what its key names by the checks every runtime read
/// makes: the last version published, or the newest version sent a counterparty, for
/// [`check_last_version`] over the whole layer. It is the app's own data, so the check
/// guards against corruption only.
pub(crate) fn check_restored(
    key: &[u8],
    value: &[u8],
    binder: &state::Binder<'_>,
) -> Result<Restored> {
    match StateKey::parse(key).ok_or_else(PaykitError::corrupt)? {
        StateKey::List => {
            state::restored::<KeptList>(key, value, binder)?;
            Ok(Restored::Other)
        }
        StateKey::LastVersion => {
            state::restored::<u64>(key, value, binder).map(Restored::LastVersion)
        }
        StateKey::ListRecipient(_) => {
            let recipient = state::restored::<Recipient>(key, value, binder)?;
            Ok(Restored::Newest(
                recipient.current().map_or(0, |o| o.version),
            ))
        }
        StateKey::Releases { .. } => {
            state::restored::<Releases>(key, value, binder)?;
            Ok(Restored::Other)
        }
        _ => Err(PaykitError::corrupt()),
    }
}

/// What a value of the publishing state holds for [`check_last_version`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Restored {
    /// The last version published.
    LastVersion(u64),
    /// The newest version sent one counterparty.
    Newest(u64),
    /// Neither.
    Other,
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
    state::cx_get::<Recipient>(cx, &key::list_recipient(sender))
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

/// The receive step's part for a `private_payment_list_request` (Paykit spec 8.4, "Answering
/// a list request"; Paykit wire 4.14), from a counterparty that gets this app's lists, about
/// this app or none:
///
/// - naming none, the newest version, which counts as used by the payer's word, one expired
///   at the request's acceptance, or one this app no longer tracks (expired, or lost by a
///   restore): a newer version goes within the 24-hour bound ([`Recipient::requested`]);
/// - naming an older version still unexpired, used or not: the newest is sent again,
///   nothing issued.
///
/// One newer version at most answers each version named: a later request naming one it
/// answered gets the newest sent again, nothing issued, so an answer lost on the way is
/// answered. A request from a counterparty the app never sent a list changes nothing.
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
    let newest = recipient.current().map(|o| o.version);
    let Some(held) = request.held_version else {
        recipient.requested = true;
        return keep_recipient(cx, &recipient);
    };
    recipient
        .named
        .retain(|(_, when)| when.saturating_add(KEPT_MESSAGE_MS) > at);
    let answered = match recipient.versions.iter().find(|o| o.version == held) {
        Some(o) => o.asked || (o.expires_at_ms > at && Some(o.version) != newest),
        None => recipient.named.iter().any(|(version, _)| *version == held),
    };
    if answered {
        recipient.resend = true;
    } else {
        recipient.requested = true;
        if recipient.asking.len() < MAX_NAMED {
            recipient.asking.insert(held);
        }
    }
    keep_recipient(cx, &recipient)
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
        if self.kept_list().await?.as_ref() == Some(&list) {
            return Ok(ListsPublished::default());
        }
        self.local_put(key::LIST, &list).await?;
        let mut active = Vec::new();
        for recipient in self.recipients().await? {
            if recipient.active {
                active.push(recipient.counterparty);
            }
        }
        self.know_deny_copy().await?;
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
    /// label; [`Refusal::Usage`] before any list is set, or for the app's own identity;
    /// [`Refusal::CounterpartyBlocked`] when the deny list holds one of them, or
    /// [`Refusal::NotNow`] while its copy isn't known and can't be read, with nothing sent to
    /// any; a store failure.
    pub async fn publish_list_to(&mut self, counterparties: &[[u8; 32]]) -> Result<ListsPublished> {
        self.refresh_own()?;
        self.require_payee()?;
        if self.kept_list().await?.is_none() {
            return Err(Refusal::Usage("Paykit spec 8.4: no list is set".into()).into());
        }
        if counterparties.contains(&self.identity()) {
            return Err(
                Refusal::Usage("Paykit spec 8.4: a list goes to another identity".into()).into(),
            );
        }
        self.refuse_held(counterparties).await?;
        self.publish_to(counterparties, false, None).await
    }

    /// Stop sending `counterparty` the app's lists, as when it is no longer a contact: it is
    /// sent a newer, empty version, which replaces the one it holds once its inbox accepts
    /// it, and gets no renewal or answer after. A version it held may still be paid until it
    /// expires, as a payment started before the empty one arrived goes on: its single-use
    /// payloads stay the payment method's to expire. [`Paykit::publish_list_to`] sends it the
    /// list again.
    ///
    /// The withdrawal goes at once even while sends to `counterparty` are held, the deny list
    /// holding it or its copy not known (Paykit spec 8.1, 8.4): it stops payments to
    /// endpoints this app no longer wants paid, and a payer this app blocked can still pay
    /// them. An empty version that doesn't reach its inbox is owed, and goes again at a later
    /// receive, a restore's included ([`Paykit::receive`]).
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`] for a counterparty that never got a list;
    /// [`Refusal::CertificateRefused`]; [`Refusal::ScopeMissing`] without `request` or a
    /// label; a store failure.
    pub async fn withdraw_list_from(&mut self, counterparty: &[u8; 32]) -> Result<ListsPublished> {
        self.refresh_own()?;
        self.require_payee()?;
        if self.recipient(counterparty).await?.is_none() {
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
        for recipient in self.recipients().await? {
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

    /// What a proof naming this app's list `version` and `endpoint` is checked against
    /// (Paykit spec 8.4, "Only what it offered"): the payload this app offered `payer` there,
    /// while it keeps the offer, until [`KEPT_MESSAGE_MS`] after the version's `expires_at`.
    /// After that `None`, the newest version an active counterparty holds included: a late
    /// proof of a real payment (Paykit spec 7.2) is checked by the payment method alone, and
    /// credited once by its settlement. Within it, [`Refusal::NotOffered`] for a version or
    /// endpoint this app didn't offer the payer. For a version it doesn't hold, the offer
    /// would still be kept while a version published then would be: its `expires_at` is at
    /// most [`LIST_LIFETIME_MS`] after its version.
    pub(crate) async fn offered_payload(
        &self,
        payer: &[u8; 32],
        version: u64,
        endpoint: &PaymentEndpointIdentifier,
    ) -> Result<std::result::Result<Option<PaymentEndpointPayload>, Refusal>> {
        let held = self.recipient(payer).await?;
        let offered = held
            .as_ref()
            .and_then(|r| r.versions.iter().find(|o| o.version == version));
        let kept = |expires_at_ms: u64| self.now() < expires_at_ms.saturating_add(KEPT_MESSAGE_MS);
        let Some(offered) = offered else {
            return Ok(if kept(version.saturating_add(LIST_LIFETIME_MS)) {
                Err(Refusal::NotOffered)
            } else {
                Ok(None)
            });
        };
        if !kept(offered.expires_at_ms) {
            return Ok(Ok(None));
        }
        match offered.endpoints.get(endpoint.as_str()) {
            Some(o) => Ok(Ok(Some(PaymentEndpointPayload::parse(&o.payload)?))),
            None => Ok(Err(Refusal::NotOffered)),
        }
    }

    /// The app's list (`ll`), read whole ([`KeptList::check`]); `None` before one is set.
    ///
    /// # Errors
    ///
    /// A store failure, or a list that doesn't read whole, as a corrupt store.
    async fn kept_list(&self) -> Result<Option<KeptList>> {
        let list = self.local_get::<KeptList>(key::LIST).await?;
        if let Some(list) = &list {
            list.check()?;
        }
        Ok(list)
    }

    /// What this app sent `counterparty` (`lp/<counterparty>`), read whole and bound to its
    /// key ([`Recipient::check`]); `None` for a counterparty sent nothing.
    ///
    /// # Errors
    ///
    /// A store failure, or a value that doesn't read whole or names another counterparty,
    /// as a corrupt store.
    pub(crate) async fn recipient(&self, counterparty: &[u8; 32]) -> Result<Option<Recipient>> {
        let held = self
            .local_get::<Recipient>(&key::list_recipient(counterparty))
            .await?;
        if let Some(recipient) = &held {
            recipient.check(counterparty)?;
        }
        Ok(held)
    }

    /// Every counterparty this app sends its lists to, or did, as [`Paykit::recipient`]
    /// reads each.
    ///
    /// # Errors
    ///
    /// As [`Paykit::recipient`], also for a key that doesn't parse.
    async fn recipients(&self) -> Result<Vec<Recipient>> {
        let mut out = Vec::new();
        for (rest, recipient) in self.local_scan::<Recipient>(key::LIST_RECIPIENTS).await? {
            let key = format!("{}{rest}", key::LIST_RECIPIENTS);
            let Some(StateKey::ListRecipient(counterparty)) = StateKey::parse(key.as_bytes())
            else {
                return Err(PaykitError::corrupt());
            };
            recipient.check(&counterparty)?;
            out.push(recipient);
        }
        Ok(out)
    }

    /// Every release kept (`rl/`), read whole and bound to its key ([`Releases::check`]).
    ///
    /// # Errors
    ///
    /// A store failure, or a key or value that doesn't read whole, or a value naming
    /// another counterparty or version than its key, as a corrupt store.
    async fn releases_kept(&self) -> Result<Vec<Releases>> {
        let mut out = Vec::new();
        for (rest, releases) in self.local_scan::<Releases>(key::RELEASES).await? {
            let key = format!("{}{rest}", key::RELEASES);
            let Some(StateKey::Releases {
                counterparty,
                version,
            }) = StateKey::parse(key.as_bytes())
            else {
                return Err(PaykitError::corrupt());
            };
            releases.check(&counterparty, version)?;
            out.push(releases);
        }
        Ok(out)
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

    /// Read the deny list again when its copy is [`crate::LISTS_REFRESH_MS`] old or
    /// isn't known ([`Paykit::refresh_lists_when_due`]): a read that fails leaves the copy
    /// unknown. Every list send is then gated on [`Paykit::sends_held`], which holds a send
    /// while the copy holds the counterparty or isn't known (Paykit spec 8.1, "Blocked
    /// counterparties"; v0.1 "Block A Peer"), so no list goes on a copy too old to judge by.
    ///
    /// # Errors
    ///
    /// A store failure or an alarm.
    async fn know_deny_copy(&mut self) -> Result<()> {
        self.refresh_lists_when_due().await
    }

    /// Why no list may go to `counterparty` now: [`Refusal::CounterpartyBlocked`] while the
    /// copy of the deny list holds it, [`Refusal::NotNow`] while the copy isn't known, or
    /// `None` when a list may go ([`Paykit::sends_held`]).
    ///
    /// # Errors
    ///
    /// A store failure.
    async fn list_held(&self, counterparty: &[u8; 32]) -> Result<Option<Refusal>> {
        if !self.sends_held(counterparty).await? {
            return Ok(None);
        }
        Ok(Some(if self.deny_copy_known().await? {
            Refusal::CounterpartyBlocked
        } else {
            Refusal::NotNow(
                "Paykit spec 8.1: no list goes while the copy of the deny list isn't known".into(),
            )
        }))
    }

    /// Refuse a call that sends `counterparties` a list while sends to one of them are held
    /// ([`Paykit::list_held`]), with nothing sent to any.
    async fn refuse_held(&mut self, counterparties: &[[u8; 32]]) -> Result<()> {
        self.know_deny_copy().await?;
        for counterparty in counterparties {
            if let Some(refusal) = self.list_held(counterparty).await? {
                return Err(refusal.into());
            }
        }
        Ok(())
    }

    /// Send each of `counterparties` a newer version, empty when `withdraw`, each on its own.
    /// One whose sends are held ([`Paykit::list_held`]) is sent nothing: a newer version is
    /// owed it, which goes once a read of the deny list shows it not blocked. A withdrawal
    /// goes all the same: it only stops payments ([`Paykit::withdraw_list_from`]). `undelivered` is as for [`Paykit::settled`].
    async fn publish_to(
        &mut self,
        counterparties: &[[u8; 32]],
        withdraw: bool,
        undelivered: Option<&[String]>,
    ) -> Result<ListsPublished> {
        let mut out = ListsPublished::default();
        for counterparty in counterparties {
            let held = if withdraw {
                None
            } else {
                self.list_held(counterparty).await?
            };
            if let Some(refusal) = held {
                let rkey = key::list_recipient(counterparty);
                if let Some(mut recipient) = self.recipient(counterparty).await? {
                    if !recipient.owed {
                        recipient.owed = true;
                        self.local_put(&rkey, &recipient).await?;
                    }
                }
                out.not_sent.push(ListNotSent {
                    counterparty: *counterparty,
                    reason: refusal.to_string(),
                });
                continue;
            }
            match self.send_list(*counterparty, withdraw, undelivered).await {
                Ok(sent) => out.sent.push(sent),
                // A store that fails, or holds state of the SDK's own that doesn't read, is
                // no list not sent: the call fails, and what is owed stays for the next one.
                Err(error @ PaykitError::Client(AppError::Store(_) | AppError::Alarm(_))) => {
                    return Err(error);
                }
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
            self.kept_list()
                .await?
                .ok_or_else(|| Refusal::Usage("Paykit spec 8.4: no list is set".into()))?
        };
        let rkey = key::list_recipient(&counterparty);
        let mut recipient = match self.recipient(&counterparty).await? {
            Some(held) => self.settled(held, undelivered).await?,
            None => Recipient::new(counterparty),
        };
        let now = self.now();
        let last = self.local_get(key::LAST_VERSION).await?;
        check_last_version(last, recipient.current().map(|o| o.version))?;
        let version = next_version(now, last);
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
            asked: false,
        });
        recipient.active = !withdraw;
        // A newer version answers every request owed, and supersedes a re-send.
        recipient.owed = false;
        recipient.resend = false;
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
    /// where one is owed, a list request asks for one within the 24-hour bound, or a renewal
    /// is due; else the newest is sent again where a request named an older one. Nothing is
    /// sent to a counterparty whose sends are held ([`Paykit::list_held`]). The releases kept are asked of
    /// the payment method last. `duties` are the duty events the receive listed after its
    /// read, which tell the messages the client gave up ([`undelivered`]).
    pub(crate) async fn tend_lists(
        &mut self,
        removed: bool,
        duties: &[AppEvent],
    ) -> Result<ListsPublished> {
        let undelivered = undelivered(duties);
        // A certificate without a selling scope sends no list: its recipients would drop it.
        let payee = paykit_lib::scopes::acts_as_payee(self.app.cert().scopes());
        let mut waiting = Vec::new();
        let mut withdrawals = Vec::new();
        let now = self.now();
        let mut tx = Tx::default();
        for recipient in self.recipients().await? {
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
            // A withdrawal whose empty version didn't reach the counterparty, as when a
            // restore's outbox lacks it, is owed while a version it held may still be paid:
            // it goes again, held sends or not, at most once an hour.
            let unreached = recipient
                .current()
                .is_some_and(|o| o.fate == Fate::Unreached);
            let payable = recipient
                .versions
                .iter()
                .any(|o| !o.endpoints.is_empty() && now < o.expires_at_ms);
            let recent = recipient
                .resent_at_ms
                .is_some_and(|at| at <= now && now - at < RESEND_INTERVAL_MS);
            if payee && !recipient.active && unreached && payable {
                if !recent {
                    withdrawals.push(recipient.counterparty);
                }
                continue;
            }
            let due = recipient.current().is_some_and(|o| renewal_due(o, now));
            let waits = recipient.owed || recipient.requested || recipient.resend || due;
            if payee && recipient.active && waits {
                waiting.push(recipient);
            }
        }
        self.local_commit(tx).await?;
        let withdrawn = self
            .publish_to(&withdrawals, true, Some(&undelivered))
            .await?;
        for sent in &withdrawn.sent {
            if let Some(mut recipient) = self.recipient(&sent.counterparty).await? {
                recipient.resent_at_ms = Some(now);
                self.local_put(&key::list_recipient(&sent.counterparty), &recipient)
                    .await?;
            }
        }
        if removed || waiting.is_empty() {
            self.run_releases().await?;
            return Ok(withdrawn);
        }
        // A counterparty whose sends are held keeps what it waits for, uncounted, until a
        // read shows it not blocked.
        self.know_deny_copy().await?;
        let mut blocked = Vec::new();
        let mut owed = Vec::new();
        let mut resend = Vec::new();
        let mut tx = Tx::default();
        for mut recipient in waiting {
            if let Some(refusal) = self.list_held(&recipient.counterparty).await? {
                blocked.push(ListNotSent {
                    counterparty: recipient.counterparty,
                    reason: refusal.to_string(),
                });
                continue;
            }
            let rkey = key::list_recipient(&recipient.counterparty);
            if recipient.requested {
                // Every newer version a list request asks for goes within the bound of any 24
                // hours. Past it the request is held, not dropped: it stays owed, with the
                // versions it named, and is answered at a later receive once the bound
                // frees.
                recipient
                    .answers
                    .retain(|at| *at <= now && now - *at < DAY_MS);
                if (recipient.answers.len() as u64) < self.config.list_answers_per_day {
                    recipient.requested = false;
                    let asking = std::mem::take(&mut recipient.asking);
                    recipient.answers.push(now);
                    recipient.owed = true;
                    for version in asking {
                        match recipient.versions.iter_mut().find(|o| o.version == version) {
                            Some(o) => o.asked = true,
                            None => recipient.named.push((version, now)),
                        }
                    }
                    let over = recipient.named.len().saturating_sub(MAX_NAMED);
                    recipient.named.drain(..over);
                }
                tx.put(&rkey, &recipient)?;
            }
            let due = recipient.current().is_some_and(|o| renewal_due(o, now));
            if recipient.owed || due {
                owed.push(recipient.counterparty);
            } else if let (true, Some(current)) = (recipient.resend, recipient.current()) {
                // The newest goes again at most once an hour; a request within it gets none.
                let recent = recipient
                    .resent_at_ms
                    .is_some_and(|at| at <= now && now - at < RESEND_INTERVAL_MS);
                if recent {
                    recipient.resend = false;
                    tx.put(&rkey, &recipient)?;
                } else {
                    resend.push((recipient.counterparty, current.clone()));
                }
            }
        }
        self.local_commit(tx).await?;
        let mut out = self.publish_to(&owed, false, Some(&undelivered)).await?;
        out.not_sent.extend(blocked);
        out.sent.extend(withdrawn.sent);
        out.not_sent.extend(withdrawn.not_sent);
        for (counterparty, current) in resend {
            if self.app.resend(&current.msg_id).await.is_err() {
                continue;
            }
            let rkey = key::list_recipient(&counterparty);
            if let Some(mut recipient) = self.recipient(&counterparty).await? {
                recipient.resend = false;
                recipient.resent_at_ms = Some(now);
                self.local_put(&rkey, &recipient).await?;
            }
            out.resent.push(SentList {
                counterparty,
                version: current.version,
                msg_id: current.msg_id,
            });
        }
        Ok(out)
    }

    /// Ask the payment method to take back every single-use payload kept to release
    /// (`rl/`), each once per call. One it doesn't take now stays kept, for the next receive.
    pub(crate) async fn run_releases(&mut self) -> Result<()> {
        for mut releases in self.releases_kept().await? {
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
            asked: false,
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

    const MSG_ID: &str = "123e4567-e89b-42d3-a456-426614174000";

    /// A version as [`Paykit::send_list`] keeps it, published at `at`.
    fn sent_at(at: u64) -> Offered {
        let mut endpoints = BTreeMap::new();
        endpoints.insert(
            "btc-bitcoin-p2wpkh".to_owned(),
            OfferedPayload {
                payload: "bc1qissued1".into(),
                single_use: true,
            },
        );
        Offered {
            msg_id: MSG_ID.into(),
            endpoints,
            ..offered(at, at + LIST_LIFETIME_MS)
        }
    }

    /// What was sent `[3; 32]`, one version.
    fn recipient() -> Recipient {
        Recipient {
            versions: vec![sent_at(1_000)],
            ..Recipient::new([3; 32])
        }
    }

    /// [`check_restored`] of `value` at `key`.
    fn check_at(key: &str, value: &[u8]) -> Result<Restored> {
        check_restored(key.as_bytes(), value, &state::Binder::unblinded([9; 32]))
    }

    fn restores(recipient: &Recipient) -> bool {
        let value = state::encode(recipient).unwrap();
        check_at(&key::list_recipient(&[3; 32]), &value).is_ok()
    }

    fn releases() -> Releases {
        let mut payloads = BTreeMap::new();
        payloads.insert("btc-bitcoin-p2wpkh".to_owned(), "bc1qissued1".to_owned());
        Releases {
            counterparty: [3; 32],
            version: 7,
            payloads,
        }
    }

    fn releases_restore(releases: &Releases) -> bool {
        check_at(
            &key::releases(&[3; 32], 7),
            &state::encode(releases).unwrap(),
        )
        .is_ok()
    }

    #[test]
    fn test_publish_restore_binds_each_value_to_its_key() {
        let value = state::encode(&recipient()).unwrap();
        assert_eq!(
            check_at(&key::list_recipient(&[3; 32]), &value).unwrap(),
            Restored::Newest(1_000)
        );
        assert!(check_at(&key::list_recipient(&[4; 32]), &value).is_err());
        let value = state::encode(&releases()).unwrap();
        let at = |counterparty: [u8; 32], version| key::releases(&counterparty, version);
        assert!(check_at(&at([3; 32], 7), &value).is_ok());
        assert!(check_at(&at([3; 32], 8), &value).is_err());
        assert!(check_at(&at([4; 32], 7), &value).is_err());
        let mut list = KeptList::default();
        list.endpoints
            .insert("btc-bitcoin-p2wpkh".into(), Some("bc1q".into()));
        assert!(check_at(key::LIST, &state::encode(&list).unwrap()).is_ok());
        list.endpoints.insert("not an identifier".into(), None);
        assert!(check_at(key::LIST, &state::encode(&list).unwrap()).is_err());
        assert_eq!(
            check_at(key::LAST_VERSION, &state::encode(&5u64).unwrap()).unwrap(),
            Restored::LastVersion(5)
        );
        assert!(check_at(key::LAST_VERSION, &state::encode(&"5").unwrap()).is_err());
    }

    #[test]
    fn test_publish_restore_takes_every_state_a_run_produces() {
        let mut sent = recipient();
        // Accepted and decided, used by a proof, a request's answer owed, renewed later.
        sent.versions[0].accepted_at_ms = Some(1_500);
        sent.versions[0].fate = Fate::Reached;
        sent.versions[0].used = true;
        sent.versions[0].asked = true;
        sent.requested = true;
        sent.asking.insert(900);
        let mut later = sent_at(2_000);
        later.version = 2_500;
        later.fate = Fate::Unreached;
        later.endpoints.clear();
        sent.versions.push(later);
        sent.active = false;
        assert!(restores(&sent));
        assert!(releases_restore(&releases()));
    }

    #[test]
    fn test_publish_restore_refuses_lists_sent_with_no_version() {
        let mut sent = recipient();
        sent.versions.clear();
        assert!(!restores(&sent));
    }

    #[test]
    fn test_publish_restore_refuses_versions_out_of_order() {
        let mut sent = recipient();
        sent.versions.push(sent_at(1_000));
        assert!(!restores(&sent));
        let mut sent = recipient();
        sent.versions.insert(0, sent_at(2_000));
        assert!(!restores(&sent));
    }

    #[test]
    fn test_publish_restore_refuses_a_version_with_a_msg_id_never_given_out() {
        let mut sent = recipient();
        sent.versions[0].msg_id = MSG_ID.to_uppercase();
        assert!(!restores(&sent));
    }

    #[test]
    fn test_publish_restore_refuses_an_expiry_not_a_lifetime_after_publishing() {
        let mut sent = recipient();
        sent.versions[0].expires_at_ms -= 1;
        assert!(!restores(&sent));
    }

    #[test]
    fn test_publish_restore_refuses_a_version_below_its_publishing_time() {
        let mut sent = recipient();
        sent.versions[0].version -= 1;
        assert!(!restores(&sent));
    }

    #[test]
    fn test_publish_restore_refuses_an_accepted_version_left_undecided() {
        let mut sent = recipient();
        sent.versions[0].accepted_at_ms = Some(1_500);
        assert!(!restores(&sent));
    }

    #[test]
    fn test_publish_restore_refuses_a_used_version_without_a_single_use_endpoint() {
        let mut sent = recipient();
        sent.versions[0].used = true;
        for endpoint in sent.versions[0].endpoints.values_mut() {
            endpoint.single_use = false;
        }
        assert!(!restores(&sent));
    }

    #[test]
    fn test_publish_restore_refuses_an_offered_endpoint_misspelled() {
        let mut sent = recipient();
        let payload = sent.versions[0]
            .endpoints
            .remove("btc-bitcoin-p2wpkh")
            .unwrap();
        sent.versions[0]
            .endpoints
            .insert("not an identifier".into(), payload);
        assert!(!restores(&sent));
        let mut sent = recipient();
        for endpoint in sent.versions[0].endpoints.values_mut() {
            endpoint.payload.clear();
        }
        assert!(!restores(&sent));
    }

    #[test]
    fn test_publish_restore_refuses_versions_named_with_no_answer_owed() {
        let mut sent = recipient();
        sent.asking.insert(900);
        assert!(!restores(&sent));
    }

    #[test]
    fn test_publish_restore_refuses_releases_with_no_payload() {
        let mut empty = releases();
        empty.payloads.clear();
        assert!(!releases_restore(&empty));
    }

    #[test]
    fn test_publish_restore_refuses_releases_misspelled() {
        let mut misspelled = releases();
        misspelled
            .payloads
            .insert("not an identifier".into(), "bc1q".into());
        assert!(!releases_restore(&misspelled));
        let mut misspelled = releases();
        misspelled
            .payloads
            .insert("btc-lightning-bolt11".into(), String::new());
        assert!(!releases_restore(&misspelled));
    }

    #[test]
    fn test_publish_last_version_never_below_a_version_sent() {
        assert!(check_last_version(None, None).is_ok());
        assert!(check_last_version(Some(5), None).is_ok());
        assert!(check_last_version(Some(5), Some(5)).is_ok());
        assert!(check_last_version(Some(5), Some(6)).is_err());
        assert!(check_last_version(None, Some(1)).is_err());
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
