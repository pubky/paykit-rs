//! Destination checks before paying (Paykit spec 4, 6, 7.2 step 1).
//!
//! Destinations come only from the payee (Paykit spec 6): an app pays only to endpoints in a
//! verified message from the payee's current app, or to the payee's public endpoints. Before
//! money moves, the payer checks, against the payee's `identity.json` looked up within the
//! last 10 minutes at its Homeserver and its switch, the higher version taken, through the
//! client's version floor and equivocation check ([`pubky_switch_client::app::App::identity_of`],
//! Switch spec 4.3):
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
//! A public endpoint's carrier is the payee app's record, read from the Homeserver
//! ([`crate::app_records`]): it names the app whose path it was read from, carries the
//! endpoint in its Paykit member, and is used only until the earlier of its `expires_at` and
//! its `updated_at` plus the identity's record age limit, and not while its `updated_at` is
//! ahead of the payer's clock (Paykit spec 4, Paykit wire 3.2).
//!
//! A removed app's destinations are ignored, even if its messages or its record are still
//! held (Paykit spec 4).
//!
//! The payee's switch is asked beside the Homeserver's reads, and waited on for at most
//! [`IDENTITY_GRACE_MS`] after them, on the host's [`crate::Timer`]; a lookup still running
//! is kept, and its answer taken at the next check before money moves (Paykit spec 4, 14).
//!
//! A request's or a Private Payment List's endpoint is a verified message's: it needs a copy
//! of the payee's `identity.json` read in the last 10 minutes, from its Homeserver or its
//! switch, and doesn't wait on the switch (Paykit spec 7.2). A public endpoint needs what
//! follows.
//!
//! The payee's `identity.json` is acted on as Switch spec 4.3 allows ([`PeerIdentity::may_act`]):
//! once its switch confirmed the copy; otherwise a payment the user started goes on only on
//! the user's explicit confirmation, and an automatic one (an executor) never does.
//!
//! Security: this is the check that stops a payment to a destination an attacker planted. It
//! fails closed: a lookup that fails refuses the payment, and an automatic payment never acts
//! on the Homeserver's copy alone (Paykit spec 7.2).
//!
//! [`PeerIdentity::may_act`]: pubky_switch_client::app::PeerIdentity::may_act

use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, PoisonError};
use std::task::{Context, Poll};

use paykit_lib::app_record::PaykitMember;
use paykit_lib::bodies::RequestDestination;
use paykit_lib::records::{DestinationSource, StoredMessage};
use paykit_lib::scopes::{acts_as_payee, check_certificate};
use paykit_lib::Body;
use pubky_switch_client::app::{
    Initiator, MayAct, PeerIdentity, SwitchAnswer, UserConfirmed, DOC_CACHE_MS,
};
use pubky_switch_client::transport::BoxFuture;
use pubky_switch_core::records::{AppCert, AppRecord, IdentityDoc, Standing};

use crate::error::{Refusal, Result};
use crate::journal::Destination;
use crate::paykit::Paykit;

/// How long a payment waits for the payee's switch to answer a lookup that is still running
/// after the Homeserver's read, before a user's payment may go on the user's confirmation
/// (Switch spec 4.3, "No added latency").
pub const IDENTITY_GRACE_MS: u64 = 2_000;

impl Paykit {
    /// The payee's `identity.json` for acting on its destinations (Paykit spec 4, 7.2; Switch
    /// spec 4.3, "The switch's copy"), as [`Paykit::payee_identity_with`] takes it, with no
    /// other reads beside the switch's lookup.
    pub(crate) async fn payee_identity(
        &mut self,
        payee: &[u8; 32],
        need: Need,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
    ) -> Result<std::result::Result<PeerIdentity, Refusal>> {
        let (identity, _) = self
            .payee_identity_with::<()>(payee, need, by, confirmed, |_| None)
            .await?;
        Ok(identity)
    }

    /// The payee's `identity.json` for acting on its destinations (Paykit spec 4, 7.2; Switch
    /// spec 4.3, "The switch's copy"): the copy held while it and its last switch lookup are
    /// fresh, else the Homeserver's copy and then the payee's switch's, through the client's
    /// version floor and equivocation check.
    ///
    /// The switch's lookup runs beside `reads`, the Homeserver reads the caller starts from
    /// the copy taken, and is waited on for at most [`IDENTITY_GRACE_MS`] after them, on the
    /// host's timer (Paykit spec 4, 14; Switch spec 4.3, "No added latency"). One still
    /// running then is kept: its answer is taken whenever it arrives before money moves, at
    /// the next check of this payee. A copy the switch confirmed is acted on. One it couldn't
    /// confirm, or hasn't yet, is acted on only for a payment the user started and confirmed
    /// (`confirmed`); an automatic payment looks it up once more and otherwise isn't made.
    /// `Err` inside holds the refusal; the reads' result comes with it.
    pub(crate) async fn payee_identity_with<R: Send + 'static>(
        &mut self,
        payee: &[u8; 32],
        need: Need,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
        reads: impl FnOnce(&IdentityDoc) -> Option<BoxFuture<'static, R>>,
    ) -> Result<(std::result::Result<PeerIdentity, Refusal>, Option<R>)> {
        let mut reads = Some(reads);
        let mut read: Option<R> = None;
        let mut refresh = false;
        loop {
            // A lookup still running from an earlier check goes on: its copy and its answer,
            // while the copy is fresh.
            let kept = self
                .lookups
                .remove(payee)
                .filter(|kept| self.now() < kept.identity.fresh_until_ms());
            let (identity, switch) = match kept {
                Some(kept) => (kept.identity, Some(kept.switch)),
                None => match self.app.begin_identity_lookup(payee, refresh).await {
                    Ok(lookup) => (lookup.identity, lookup.switch.map(PendingLookup::new)),
                    Err(error) => {
                        return Ok((
                            Err(Refusal::IdentityUnavailable(format!(
                                "Switch spec 4.3: the payee's identity.json can't be read now: \
                                 {error}"
                            ))),
                            read,
                        ))
                    }
                },
            };
            let started = reads.take().and_then(|start| start(&identity.doc));
            let identity = match switch {
                None => {
                    if let Some(started) = started {
                        read = Some(started.await);
                    }
                    identity
                }
                Some(switch) => {
                    // A destination a verified message carries doesn't wait on the switch:
                    // its answer is taken only if it is there already.
                    let sleep = match need {
                        Need::SwitchCopy => self.timer.sleep(IDENTITY_GRACE_MS),
                        Need::FreshCopy => Box::pin(async {}),
                    };
                    let (lookup, done) = race(switch, started, sleep).await;
                    if done.is_some() {
                        read = done;
                    }
                    match lookup {
                        Lookup::Answered(answer) => {
                            match self.app.finish_identity_lookup(answer).await {
                                Ok(identity) => identity,
                                Err(error) => return Ok((Err(not_taken(&error)), read)),
                            }
                        }
                        Lookup::Running(switch) => {
                            self.lookups.insert(
                                *payee,
                                KeptLookup {
                                    identity: identity.clone(),
                                    switch,
                                },
                            );
                            identity
                        }
                    }
                }
            };
            if need == Need::FreshCopy {
                // A copy read in the last 10 minutes, from the Homeserver or the switch, is
                // the one acted on (Paykit spec 7.2); the switch's confirmation isn't needed.
                let fresh = self
                    .now()
                    .checked_sub(identity.read_at_ms)
                    .is_some_and(|age| age < DOC_CACHE_MS);
                if fresh {
                    return Ok((Ok(identity), read));
                }
                if !refresh {
                    refresh = true;
                    continue;
                }
                return Ok((
                    Err(Refusal::IdentityUnavailable(
                        "Paykit spec 7.2: no copy of the payee's identity.json read in the last \
                         10 minutes"
                            .into(),
                    )),
                    read,
                ));
            }
            match identity.may_act(self.now(), by, IDENTITY_GRACE_MS) {
                MayAct::Now => return Ok((Ok(identity), read)),
                MayAct::IfUserConfirms if confirmed.is_some() => return Ok((Ok(identity), read)),
                MayAct::IfUserConfirms => return Ok((Err(Refusal::IdentityUnconfirmed), read)),
                MayAct::No | MayAct::LookUpAgain | MayAct::WaitForLookup | MayAct::WaitUntil(_)
                    if !refresh =>
                {
                    refresh = true;
                }
                _ => {
                    return Ok((
                        Err(Refusal::IdentityUnavailable(
                            "Switch spec 4.3: no fresh copy of the payee's identity.json that \
                             its switch confirmed; an automatic payment never acts on the \
                             Homeserver's copy alone"
                                .into(),
                        )),
                        read,
                    ))
                }
            }
        }
    }

    /// The checks of a destination before money moves (Paykit spec 4, 7.2): the payee's
    /// `identity.json` that passed them, as the lookup found it, or the refusal.
    pub(crate) async fn check_destination(
        &mut self,
        payee: &[u8; 32],
        destination: &Destination,
        carrier: &Carrier,
        request_hash: Option<&[u8; 32]>,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
    ) -> Result<std::result::Result<PeerIdentity, Refusal>> {
        if let Err(why) = carries(payee, destination, carrier, request_hash, self.now()) {
            return Ok(Err(Refusal::Destination(why)));
        }
        // Switch spec 4.3 asks for the switch's confirmation for public endpoints only; a
        // request's or a list's endpoint is the payee's verified message (Paykit spec 7.2).
        let identity = match self
            .payee_identity(payee, Need::of(carrier), by, confirmed)
            .await?
        {
            Ok(identity) => identity,
            Err(refusal) => return Ok(Err(refusal)),
        };
        // The lookup awaited the network: the destination is judged again on the clock now.
        Ok(self
            .destination_valid(payee, destination, carrier, request_hash, &identity.doc)
            .map(|()| identity))
    }

    /// The payee's copy of `identity.json` may still be acted on now, on the clock (Paykit
    /// spec 4, 7.2; Switch spec 4.3): read in the last 10 minutes, for a destination a
    /// verified message carries; for a public endpoint, as the switch's rules let `by`
    /// act, `confirmed` standing for the user's confirmation.
    pub(crate) fn payee_copy_usable(
        &self,
        identity: &PeerIdentity,
        need: Need,
        by: Initiator,
        confirmed: Option<UserConfirmed>,
    ) -> bool {
        let now = self.now();
        match need {
            Need::FreshCopy => now
                .checked_sub(identity.read_at_ms)
                .is_some_and(|age| age < DOC_CACHE_MS),
            Need::SwitchCopy => match identity.may_act(now, by, IDENTITY_GRACE_MS) {
                MayAct::Now => true,
                MayAct::IfUserConfirms => confirmed.is_some(),
                _ => false,
            },
        }
    }

    /// The payee's switch lookup kept from an earlier check, taken if its answer has come,
    /// without waiting for one that hasn't (Paykit spec 4): the copy it gives, or `None`
    /// when none was kept or it still runs, kept again. `Err` inside: the client refused
    /// the answer, and nothing is paid on it.
    pub(crate) async fn take_kept_lookup(
        &mut self,
        payee: &[u8; 32],
    ) -> Result<std::result::Result<Option<PeerIdentity>, Refusal>> {
        let Some(kept) = self.lookups.remove(payee) else {
            return Ok(Ok(None));
        };
        match race::<()>(kept.switch, None, Box::pin(async {})).await {
            (Lookup::Answered(answer), _) => match self.app.finish_identity_lookup(answer).await {
                Ok(identity) => Ok(Ok(Some(identity))),
                // An answer the client refuses to take is no answer to ignore: it may show
                // the payee's identity.json equivocating, and then nothing is paid to it
                // (Switch wire 3.3). It refuses as it does anywhere else.
                Err(error) => Ok(Err(not_taken(&error))),
            },
            (Lookup::Running(switch), _) => {
                self.lookups.insert(
                    *payee,
                    KeptLookup {
                        identity: kept.identity,
                        switch,
                    },
                );
                Ok(Ok(None))
            }
        }
    }

    /// Whether the payee's kept switch lookup has answered, polled once without waiting
    /// (the payment fence, [`Paykit::payment_fence`]). An answer is kept, for the next check
    /// to take: taking it is an await.
    pub(crate) fn kept_lookup_answered(&mut self, payee: &[u8; 32]) -> bool {
        use std::task::Waker;
        let Some(kept) = self.lookups.get_mut(payee) else {
            return false;
        };
        let mut cx = Context::from_waker(Waker::noop());
        match Pin::new(&mut kept.switch).poll(&mut cx) {
            Poll::Ready(answer) => {
                kept.switch = PendingLookup::new(std::future::ready(answer));
                true
            }
            Poll::Pending => false,
        }
    }

    /// The checks of a destination that need no network, on the clock now and the payee's
    /// `identity.json` taken (Paykit spec 4, 5, 7.2): the carrier carries it and hasn't
    /// expired, and its app is current. Run again after every await before money moves.
    pub(crate) fn destination_valid(
        &self,
        payee: &[u8; 32],
        destination: &Destination,
        carrier: &Carrier,
        request_hash: Option<&[u8; 32]>,
        doc: &IdentityDoc,
    ) -> std::result::Result<(), Refusal> {
        let now = self.now();
        carries(payee, destination, carrier, request_hash, now).map_err(Refusal::Destination)?;
        current(doc, &destination.app_id, carrier, now).map_err(Refusal::Destination)
    }
}

/// The refusal of a switch answer the client wouldn't take, such as two documents at one
/// version, equivocation (Switch wire 3.3), for which the client also raises
/// `AppEvent::PeerEquivocation` for the host to alert the user, and refuses every later
/// lookup of that identity.
fn not_taken(error: &pubky_switch_client::app::AppError) -> Refusal {
    Refusal::IdentityUnavailable(format!(
        "Switch spec 4.3: the payee's identity.json can't be taken: {error}"
    ))
}

/// What a destination needs of the payee's `identity.json` (Paykit spec 4, 7.2; Switch spec
/// 4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Need {
    /// A public endpoint: the copy the payee's switch confirmed, waited on through a grace,
    /// or the user's explicit confirmation without it.
    SwitchCopy,
    /// A destination a verified message carries: a copy read in the last 10 minutes.
    FreshCopy,
}

impl Need {
    /// What `carrier`'s destination needs.
    pub(crate) fn of(carrier: &Carrier) -> Self {
        match carrier {
            Carrier::Message(_) => Self::FreshCopy,
            Carrier::Record(_) => Self::SwitchCopy,
        }
    }
}

/// A payee's switch lookup still running after its grace, with the copy it began from. An
/// answer the payment fence saw come is kept in it, as a lookup that answers at once.
pub(crate) struct KeptLookup {
    pub(crate) identity: PeerIdentity,
    pub(crate) switch: PendingLookup,
}

/// A switch lookup's answer to come.
///
/// The boxed future is `Send` but not `Sync`, and [`Paykit`] keeps it, so it sits in a
/// `Mutex`: `Paykit` is then `Sync`, and the future of a call awaiting a `&self` method is
/// `Send`, for a host to spawn on a multi-threaded runtime. The `Mutex` is never locked: the
/// future is polled only through `&mut`.
pub(crate) struct PendingLookup(Mutex<BoxFuture<'static, SwitchAnswer>>);

impl PendingLookup {
    fn new(future: impl Future<Output = SwitchAnswer> + Send + 'static) -> Self {
        Self(Mutex::new(Box::pin(future)))
    }
}

impl Future for PendingLookup {
    type Output = SwitchAnswer;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<SwitchAnswer> {
        // Never locked, so never poisoned; a poisoned `Mutex` would still hold the future.
        let future = self
            .get_mut()
            .0
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        future.as_mut().poll(cx)
    }
}

/// Where a switch lookup stands after [`race`].
enum Lookup {
    Answered(SwitchAnswer),
    Running(PendingLookup),
}

/// Run the switch's lookup beside `reads` until they finish, then wait on it at most until
/// `grace` resolves (Switch spec 4.3, "No added latency"). Both are polled in turn on the
/// caller's task: nothing is spawned, and a lookup not answered is handed back to be kept.
async fn race<R>(
    mut switch: PendingLookup,
    reads: Option<BoxFuture<'static, R>>,
    mut grace: BoxFuture<'static, ()>,
) -> (Lookup, Option<R>) {
    let mut answer = None;
    let mut done = None;
    if let Some(mut reads) = reads {
        std::future::poll_fn(|cx| {
            if answer.is_none() {
                if let Poll::Ready(a) = Pin::new(&mut switch).poll(cx) {
                    answer = Some(a);
                }
            }
            match reads.as_mut().poll(cx) {
                Poll::Ready(value) => {
                    done = Some(value);
                    Poll::Ready(())
                }
                Poll::Pending => Poll::Pending,
            }
        })
        .await;
    }
    if let Some(answer) = answer {
        return (Lookup::Answered(answer), done);
    }
    let answered = std::future::poll_fn(|cx| {
        if let Poll::Ready(a) = Pin::new(&mut switch).poll(cx) {
            return Poll::Ready(Some(a));
        }
        grace.as_mut().poll(cx).map(|()| None)
    })
    .await;
    match answered {
        Some(answer) => (Lookup::Answered(answer), done),
        None => (Lookup::Running(switch), done),
    }
}

/// What carries a destination (Paykit spec 4, 7.2): a verified message from the payee, a
/// request or a Private Payment List, or the payee app's verified record, for a public
/// endpoint.
#[derive(Clone, Debug)]
pub(crate) enum Carrier {
    /// A request or a Private Payment List.
    Message(Box<StoredMessage>),
    /// An app record, read from the Homeserver.
    Record(Box<AppRecord>),
}

impl Carrier {
    /// The certificate the carrier came with.
    fn cert(&self) -> &AppCert {
        match self {
            Self::Message(message) => message.cert(),
            Self::Record(record) => record.cert(),
        }
    }

    /// When the certificate must count: when the inbox accepted the message, or now for a
    /// record, read now.
    fn counts_at(&self, now: u64) -> u64 {
        match self {
            Self::Message(message) => message.accepted_at().unwrap_or(now),
            Self::Record(_) => now,
        }
    }
}

/// The carrier carries the destination, names the obligation, and hasn't expired (Paykit
/// spec 4, 5, 7.2). A record's age limit needs the identity's `identity.json`, and is checked
/// with the app's standing ([`current`]).
pub(crate) fn carries(
    payee: &[u8; 32],
    destination: &Destination,
    carrier: &Carrier,
    request_hash: Option<&[u8; 32]>,
    now: u64,
) -> std::result::Result<(), String> {
    let carrier = match carrier {
        Carrier::Message(message) => message,
        Carrier::Record(record) => return record_carries(payee, destination, record, now),
    };
    if &carrier.sender() != payee {
        return Err("Paykit spec 6: the destination comes from another identity".into());
    }
    if carrier.message().from.app_id != destination.app_id {
        return Err("Paykit wire 4.4: the destination isn't owned by the sending app".into());
    }
    let endpoint_id = destination.endpoint_id().map_err(|e| e.to_string())?;
    match (
        destination.source().map_err(|e| e.to_string())?,
        carrier.body(),
    ) {
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

/// A public endpoint's record carries it (Paykit spec 4, Paykit wire 3.2): the payee's
/// record of the app the destination names, whose Paykit member reads and lists the
/// endpoint with its payload, that has its `expires_at` after `now` and whose `updated_at`
/// isn't ahead of the payer's clock.
fn record_carries(
    payee: &[u8; 32],
    destination: &Destination,
    record: &AppRecord,
    now: u64,
) -> std::result::Result<(), String> {
    if destination.source().map_err(|e| e.to_string())? != DestinationSource::Public {
        return Err("Paykit spec 7.2: no verified message carries the destination".into());
    }
    if &record.body().identity != payee {
        return Err("Paykit spec 6: the destination comes from another identity".into());
    }
    if record.body().app_id != destination.app_id {
        return Err("Paykit wire 3.2: the record isn't the destination app's".into());
    }
    let member = PaykitMember::read(record).map_err(|e| e.to_string())?;
    let endpoint_id = destination.endpoint_id().map_err(|e| e.to_string())?;
    if member
        .public_endpoints
        .get(&endpoint_id)
        .is_none_or(|p| p.as_str() != destination.payload)
    {
        return Err("Paykit spec 7.2: the record doesn't carry the destination".into());
    }
    if record.is_ahead(now) {
        return Err("Paykit spec 4: the record's updated_at is ahead of the clock".into());
    }
    if record.expires_at().is_none_or(|at| at <= now) {
        return Err("Paykit spec 4: the record has expired".into());
    }
    Ok(())
}

/// The app `app_id` that sent the carrier, or whose record it is, is current in the payee's
/// `identity.json`, with a certificate that passes Paykit's verifiers' checks (Paykit wire
/// 3.1: its scope rules, and permissions with none missing and, without another protocol's
/// scope, no other), allows `paykit_request` and, when listed, hasn't expired; a record is also
/// within the identity's record age limit (Paykit spec 4). The Switch takes a certificate
/// Paykit's verifiers refuse, so a record's, which nothing else checks, is checked here.
pub(crate) fn current(
    doc: &IdentityDoc,
    app_id: &[u8; 16],
    carrier: &Carrier,
    now: u64,
) -> std::result::Result<(), String> {
    let cert = carrier.cert();
    check_certificate(cert).map_err(|error| error.to_string())?;
    if !acts_as_payee(cert.scopes()) {
        return Err("Paykit spec 4: the sending app's certificate doesn't allow request".into());
    }
    let standing = doc.standing(app_id, &cert.hash());
    if standing == Standing::Listed && cert.expired_at(now) {
        return Err("Paykit spec 4: the sending app's listed certificate has expired".into());
    }
    let at = carrier.counts_at(now);
    if doc.listed_cert(app_id).is_none() || !standing.counts_at(at) {
        return Err(
            "Paykit spec 4: the sending app isn't current in the payee's identity.json".into(),
        );
    }
    // The record age limit is the identity's (Paykit spec 4): a record is used only until
    // its `updated_at` plus `record_max_age`, whatever `expires_at` it signed.
    if let Carrier::Record(record) = carrier {
        if now >= record.trusted_until(doc.record_max_age_ms() / 1000) {
            return Err("Paykit spec 4: the record is past the identity's record age limit".into());
        }
    }
    Ok(())
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
            Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]),
            Party::new("bob", "shop", &["paykit_request"]),
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
            carries(
                &bob.id(),
                &dest,
                &Carrier::Message(Box::new(request.clone())),
                Some(&request.hash()),
                NOW
            ),
            Ok(())
        );
    }

    #[test]
    fn test_destination_refuses_another_identity_app_payload_or_request() {
        let (alice, bob) = parties();
        let carol = Party::new("carol", "shop", &["paykit_request"]);
        let request = bob.message(&alice, "paykit.payment_request", vec![], request_body());
        let dest = destination(
            &bob,
            "btc-lightning-bolt11",
            "lnbc1",
            DestinationSource::Request,
        );
        assert!(carries(
            &carol.id(),
            &dest,
            &Carrier::Message(Box::new(request.clone())),
            None,
            NOW
        )
        .is_err());
        let other_app = destination(
            &carol,
            "btc-lightning-bolt11",
            "lnbc1",
            DestinationSource::Request,
        );
        assert!(carries(
            &bob.id(),
            &other_app,
            &Carrier::Message(Box::new(request.clone())),
            None,
            NOW
        )
        .is_err());
        let other_payload = destination(
            &bob,
            "btc-lightning-bolt11",
            "lnbc2",
            DestinationSource::Request,
        );
        assert!(carries(
            &bob.id(),
            &other_payload,
            &Carrier::Message(Box::new(request.clone())),
            None,
            NOW
        )
        .is_err());
        assert!(carries(
            &bob.id(),
            &dest,
            &Carrier::Message(Box::new(request.clone())),
            Some(&[9; 32]),
            NOW
        )
        .is_err());
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
        assert_eq!(
            carries(
                &bob.id(),
                &dest,
                &Carrier::Message(Box::new(list.clone())),
                None,
                NOW
            ),
            Ok(())
        );
        assert!(carries(
            &bob.id(),
            &dest,
            &Carrier::Message(Box::new(list.clone())),
            None,
            NOW + 3_600_000
        )
        .is_err());
        let mut other_version = dest.clone();
        other_version.list_version = Some(8);
        assert!(carries(
            &bob.id(),
            &other_version,
            &Carrier::Message(Box::new(list.clone())),
            None,
            NOW
        )
        .is_err());
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
        assert_eq!(
            current(
                &bob.doc(&[&bob]),
                &dest.app_id,
                &Carrier::Message(Box::new(request.clone())),
                NOW
            ),
            Ok(())
        );
        // Not listed.
        assert!(current(
            &bob.doc(&[]),
            &dest.app_id,
            &Carrier::Message(Box::new(request.clone())),
            NOW
        )
        .is_err());
    }

    #[test]
    fn test_destination_record_carrier_with_wrong_permissions_refused() {
        use pubky_switch_core::scopes::Permission;
        // A public record's certificate is checked in full before it carries a destination
        // (Paykit wire 3.1): permissions missing one `request` needs, or holding another.
        let missing = Party::with_permissions("bob", "shop", &["paykit_request"], &[]);
        let extra = Party::with_permissions(
            "bob",
            "shop",
            &["paykit_request"],
            &[Permission::ListsRead, Permission::ConversationAdd],
        );
        for app in [&missing, &extra] {
            let carrier = Carrier::Record(Box::new(app.record(&["paykit.payment_request"])));
            let refused = current(&app.doc(&[app]), &app.app_id(), &carrier, NOW).unwrap_err();
            assert!(refused.contains("permissions"), "{refused}");
        }
        let valid = Party::new("bob", "shop", &["paykit_request"]);
        let carrier = Carrier::Record(Box::new(valid.record(&["paykit.payment_request"])));
        assert_eq!(
            current(&valid.doc(&[&valid]), &valid.app_id(), &carrier, NOW),
            Ok(())
        );
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
        assert!(current(
            &removed,
            &dest.app_id,
            &Carrier::Message(Box::new(request.clone())),
            NOW
        )
        .is_err());
        // A request from an app without `paykit_request` carries no destination: the reader refuses
        // to store it at all (Paykit wire 5.2, the sender's side of check 8).
        let payer_app = Party::new("bob", "wallet", &["paykit_pay", "paykit_contacts"]);
        assert!(payer_app
            .try_message(&alice, "paykit.payment_request", vec![], request_body())
            .is_err());
    }
}
