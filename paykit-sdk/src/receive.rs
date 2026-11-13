//! Receiving (Paykit spec 6, 7.3, 8.1, 8.4, 9; Paykit wire 5.3, 5.4): the effect of each
//! Paykit message that passed the recipient checks, built in the client's receive step so
//! that the record write it causes and the cursor move past it go in one batch (Switch spec
//! 8.2).
//!
//! The client runs check 10 first ([`crate::PaykitProtocol`]'s `check_body`); then [`effect`]
//! adds, through the client's
//! [`ReceiveContext`], the writes the message causes:
//!
//! - **payer side** (`pay`): a request opens its request record; a payee's cancellation, a
//!   receipt and a Private Payment List go into theirs. A payee's cancellation is kept,
//!   never written in the receive step: the app places it in the request record afterwards
//!   (the kept-copy stop, Paykit spec 8.1). A receipt also settles the attempt
//!   it names, by evidence (Switch spec 10.2 row 3), in the same batch: only the payee's yes
//!   counts (Paykit spec 7.3).
//! - **payee side** (`request` or a label): the payer's acceptance, rejection, cancellation
//!   and proof go into the request record under the request's label, or, for a request
//!   whose record the app doesn't hold yet, are kept until it does (`p/pr/`); a proof of a
//!   payment without a request into the record of its settlement, so one settlement is
//!   credited once (Paykit spec 8.4).
//! - a `resend_request` is kept, with the cursor move, for [`crate::Paykit::receive`] to
//!   answer (Paykit spec 9): answering sends messages, which no batch carries.
//!
//! The spam rule (Paykit wire 4.1, Switch spec 7) gates every message that would create
//! state, at an app holding `contacts`: a request, a list, a cancellation or a proof that no
//! held record matches, and a `resend_request`. From a sender the user blocked or the
//! service flagged it is only counted; while the app's copy of the lists isn't known to be
//! complete, it waits on the feed until the lists are read. A message that updates a record
//! the app holds, such as a receipt for a payment in progress, is applied whoever sent it.
//! An app without `contacts` can't read the lists, and gates nothing.
//!
//! Every write builds on the record the app holds. A record that already reflects a message
//! was written by another own app: nothing more is written, and only the cursor moves. When
//! another own app wrote the record meanwhile, the batch fails `409 conflict`, the client
//! reads the record again and calls the protocol once more, and the effect is built again
//! on what the switch holds.

use paykit_lib::bodies::ResendRequest;
use paykit_lib::derive::{natural_key, request_obligation_blinded};
use paykit_lib::records::{
    ActivityRecord, PaymentListRecord, StoredMessage, UnrequestedDestination,
};
use paykit_lib::{AttemptId, Body};
use pubky_switch_client::app::{
    AppError, ClaimFetch, ClaimOp, LocalWrite, Need, ObligationDeadline, ReceiveContext,
    RecordContent, Refusal as ClientRefusal, StoreFailure, Unopened,
};
use pubky_switch_client::error::StoreError;
use pubky_switch_core::claims::{Basis, ClaimStatus, ClaimView, Outcome, RECONCILIATION_MS};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::records::AppCert;
use pubky_switch_core::scopes::Permission;
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::activity::{self, ActivityTarget, Role};
use crate::hold::RecordSide;
use crate::journal::JournalEntry;
use crate::marks::{count_dropped, MarkFamily};
use crate::notices::{KeptNotice, NoticeKind};
use crate::paykit::Paykit;
use crate::protocol::writes_payee_label;
use crate::records::{list_target, KeptMessage};
use crate::state::{self, key, Tx};

/// Why an effect wasn't built.
pub(crate) enum Fail {
    /// A record write can't be built now, for want of a scope key. Only the receive
    /// context's write gives it, and the context then holds the entry itself, whatever the
    /// step returns (Switch spec 3.5): the SDK never makes one of its own.
    NotNow,
    /// The effect can't be built: the entry is dropped, with the rule.
    Drop(String),
    /// The effect needs these claims read from the switch first (Switch spec 10.2): the
    /// client reads them and calls the step again.
    Need(Vec<Need>),
    /// The store failed, or holds data of the SDK's own that doesn't read: the entry is
    /// held, or parked, never consumed (Switch spec 8.2).
    Store(StoreFailure),
    /// The message's record holds a value sealed at data epoch `data_epoch`, whose scope key
    /// the app doesn't hold yet ([`wait_for_key`]): the entry waits for the key in the
    /// client's store (`Outcome::AwaitKey`, `PaykitProtocol`'s receive step).
    AwaitsKey {
        /// The data epoch the value is sealed at.
        data_epoch: u64,
    },
}

impl Fail {
    /// Local data of the SDK's own that doesn't read: retrying on the feed can't mend it,
    /// so the entry is parked at once.
    pub(crate) fn corrupt() -> Self {
        Self::Store(StoreFailure::of_entry(StoreError::Corrupt))
    }
}

impl From<AppError> for Fail {
    fn from(error: AppError) -> Self {
        match error {
            AppError::Refused(ClientRefusal::NotNow(_)) => Self::NotNow,
            // A store that fails for now, or holds a value that doesn't read, holds the
            // entry: a valid message is never consumed by a store failure (Switch spec 8.2).
            AppError::Store(error) => Self::Store(StoreFailure::from(error)),
            _ => Self::Drop("the message's effect can't be built".into()),
        }
    }
}

impl From<paykit_lib::Error> for Fail {
    fn from(_: paykit_lib::Error) -> Self {
        Self::Drop("the message's record doesn't serialize".into())
    }
}

/// A `resend_request` received and still to be answered (`p/ans/<seq>/<msg_id>`).
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct QueuedAnswer {
    /// The `seq` and `msg_id` it was read at, which its key names.
    pub(crate) seq: u64,
    pub(crate) msg_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) requester: [u8; 32],
    pub(crate) cert: SignedDoc,
    pub(crate) body: Map<String, Value>,
}

impl std::fmt::Debug for QueuedAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("QueuedAnswer(..)")
    }
}

/// The writes `stored`, a message that passed check 10, causes, added to `cx` (module docs).
/// The settlement ID of a proof's `proof` object, by the app's rail
/// ([`crate::Rail::settlement_id`]).
pub(crate) type Settlement<'a> = &'a paykit_lib::derive::SettlementIdReader<'a>;

pub(crate) fn effect(
    cx: &mut ReceiveContext<'_>,
    stored: &StoredMessage,
    seq: u64,
    settlement: Settlement<'_>,
) -> Result<(), Fail> {
    // No Paykit effect under an own certificate Paykit refuses, such as a renewal's the
    // Switch took (Paykit wire 3.1): the entry is held, never consumed, and tried again at
    // later reads, once a certificate that passes replaces it.
    if paykit_lib::scopes::check_certificate(cx.cert()).is_err() {
        return Err(Fail::Store(StoreFailure::unavailable(
            StoreError::Unavailable,
        )));
    }
    let sender = stored.sender();
    let scopes = cx.cert().scopes().to_vec();
    let pay = scopes.iter().any(|s| s == "pay");
    let payee = paykit_lib::scopes::acts_as_payee(&scopes);
    note_cursor_time(cx, stored)?;
    match stored.body() {
        Body::PaymentRequest(_) if pay => {
            let target =
                ActivityTarget::payer(natural_key::request_record(&sender, &stored.hash()));
            let held = held_or_unlisted(cx, &target, RecordSide::Payer)?;
            if held.is_some() || admits_new_state(cx, &sender, &scopes)? {
                place(cx, &target, sender, Role::Request, stored)?;
                note_peer(cx, &sender, stored)?;
            }
        }
        Body::PaymentRequestAcceptance(_)
        | Body::PaymentRequestRejection(_)
        | Body::PaymentProof(_)
            if payee =>
        {
            payer_reply(cx, stored, &scopes, settlement)?;
        }
        Body::PaymentRequestCancellation(_) => {
            let Some(request) = stored.message().refs.first().copied() else {
                return Ok(());
            };
            let payer_target =
                ActivityTarget::payer(natural_key::request_record(&sender, &request));
            // A payee's cancellation is kept, the kept-copy stop (Paykit spec 8.1): the
            // receive step writes no record for it. The app places it in the request record
            // itself, as a later write of its own, and every new attempt reads the kept copy,
            // so a stop that can't be placed still refuses the payment. One for a request
            // whose record the app holds, open or sealed under a scope key it doesn't hold
            // yet, always applies and is kept at once: keeping it needs only the record's
            // key, so the stop never waits for the value's key or a listing, and no give-up
            // of the entry can lose it.
            if pay && held_request(cx, &payer_target)? {
                return keep_cancellation(cx, &payer_target, stored, true);
            }
            // A payer's cancellation of a request this app holds goes into its record. One
            // naming a request held on neither side is new state, which the spam rule gates
            // once: at an app holding `pay`, it is kept as the stop, which no payment passes;
            // at one taking payments, as a payer's reply until the record comes (Paykit spec
            // 8.1, "Replies before their record"). An app holding both keeps both, each
            // counted only in its own bounds: the message may be a payee's cancellation of a
            // request to this identity, or a payer's of one another own app sent.
            let label = stored.body().label().cloned();
            let payee_target = ActivityTarget::payee(
                natural_key::request_record(&cx.identity(), &request),
                label.as_ref(),
            );
            let payee_held = payee && cx.record(&payee_target.record_target())?.is_some();
            if !pay || payee_held {
                if payee {
                    payer_reply(cx, stored, &scopes, settlement)?;
                }
                return Ok(());
            }
            // While the copy of a side may lack records, the absence decides nothing: the
            // entry is held.
            let content = cx
                .record(&payer_target.record_target())?
                .map(|held| held.content);
            if !holds_record(content.as_ref()) {
                hold_while_unlisted(cx, RecordSide::Payer)?;
            }
            let reply = payee && writes_payee_label(&scopes, label.as_ref());
            if reply {
                hold_while_unlisted(cx, RecordSide::Payee)?;
            }
            if admits_new_state(cx, &sender, &scopes)? {
                keep_cancellation(cx, &payer_target, stored, false)?;
                if reply {
                    keep_reply(cx, &payee_target, stored)?;
                }
            }
        }
        Body::PaymentReceipt(receipt) if pay => {
            receipt_effect(cx, stored, receipt.attempt_id)?;
        }
        Body::PrivatePaymentList(_) if pay => {
            let target = list_target(&sender, &stored.message().from.app_id);
            let record = cx.record(&target)?;
            if !holds_record(record.as_ref().map(|held| &held.content)) {
                hold_while_unlisted(cx, RecordSide::Payer)?;
            }
            let held = record.is_some_and(|held| held.content.value().is_some());
            if held || admits_new_state(cx, &sender, &scopes)? {
                list_effect(cx, stored)?;
                note_peer(cx, &sender, stored)?;
            }
        }
        Body::ResendRequest(_) => {
            if !admits_new_state(cx, &sender, &scopes)? {
                return Ok(());
            }
            let queued = QueuedAnswer {
                seq,
                msg_id: stored.message().msg_id.clone(),
                requester: sender,
                cert: stored.cert().signed().clone(),
                body: stored.message().body.clone(),
            };
            keep_local(
                cx,
                local(&key::answer(seq, &stored.message().msg_id), &queued)?,
            );
        }
        // A publishing app answers a request naming a used version with its current list
        // (Paykit spec 8.4, "Publishing"). This SDK publishes no list at all yet, so the
        // request is taken with no effect; the answer comes with list publishing.
        Body::PrivatePaymentListRequest(_) => {}
        _ => {}
    }
    Ok(())
}

fn local<T: Serialize>(key: &str, value: &T) -> Result<LocalWrite, Fail> {
    state::local_put(key, value).map_err(|_| Fail::corrupt())
}

/// Add `write` to the step's local writes. Past the step's bound
/// ([`pubky_switch_client::app::MAX_STEP_LOCAL_BYTES`]) the context refuses it and marks
/// the step; the step goes on, and the client stores the entry as too large once the step
/// applies (Switch spec 8.2). Dropping the message here instead would consume a valid
/// entry, so the refusal is not passed on.
fn keep_local(cx: &mut ReceiveContext<'_>, write: LocalWrite) {
    if let Err(error) = cx.local(write) {
        debug_assert!(
            matches!(error, AppError::Refused(ClientRefusal::Usage(_))),
            "{error:?}"
        );
    }
}

/// Keep the acceptance time of the latest Paykit entry the receive step passed, with the
/// cursor move: a backup carries it, and a restore asks the counterparties to re-send from
/// it (`hold.rs`, Paykit spec 7.2).
fn note_cursor_time(cx: &mut ReceiveContext<'_>, stored: &StoredMessage) -> Result<(), Fail> {
    let Some(at) = stored.accepted_at() else {
        return Ok(());
    };
    let held = state::cx_get::<u64>(cx, key::CURSOR_TIME)?.unwrap_or(0);
    if at > held {
        keep_local(cx, local(key::CURSOR_TIME, &at)?);
    }
    Ok(())
}

/// Whether the spam rule (Paykit wire 4.1, Switch spec 7) lets a message from `sender`
/// create state, asked only of a message no held record matches. An app without `contacts`
/// can't read the lists, and gates nothing. At one holding it, a message from a sender the
/// user blocked or the service flagged is only counted, in the effect's batch; while the
/// app's copy of the lists isn't known to be complete, the message waits on the feed
/// ([`Fail::Store`], unavailable for now) until the lists are read. The copy isn't known to
/// be complete when the client says so (`lists_as_of_ms: None`), or when the SDK's due read
/// of the lists failed ([`key::LISTS_READ`] absent): a copy older than the refresh interval
/// may miss a flag, and [`Paykit::refresh_lists_when_due`] reads the lists again before the
/// next read of the feed. A failed store read holds the message too: it is never read as a
/// clean sender. A block or unblock of `sender` this app wrote whose batch the client keeps
/// unanswered (`bk/`) counts at once, as for a payment (Paykit spec 8.1): the client's copy
/// of the lists shows it only once the list does.
fn admits_new_state(
    cx: &mut ReceiveContext<'_>,
    sender: &[u8; 32],
    scopes: &[String],
) -> Result<bool, Fail> {
    if !reads_lists(scopes) {
        return Ok(true);
    }
    let unknown = || Fail::Store(StoreFailure::unavailable(StoreError::Unavailable));
    if state::cx_get::<u64>(cx, key::LISTS_READ)?.is_none() {
        return Err(unknown());
    }
    let standing = cx.sender_disposition(sender)?;
    if standing.lists_as_of_ms.is_none() {
        return Err(unknown());
    }
    let blocked = state::cx_get::<bool>(cx, &key::own_block(sender))?.unwrap_or(standing.blocked);
    if !(blocked || standing.flagged) {
        return Ok(true);
    }
    let kept = key::spam(sender);
    let count = state::cx_get::<u64>(cx, &kept)?.unwrap_or(0);
    keep_local(cx, local(&kept, &count.saturating_add(1))?);
    Ok(false)
}

/// Whether an app with `scopes` reads the lists: `contacts` gives `lists.read`.
pub(crate) fn reads_lists(scopes: &[String]) -> bool {
    scopes.iter().any(|s| s == "contacts")
}

/// Note a counterparty with Paykit activity, for re-sends after a `missed` item or a
/// provider loss (Paykit spec 9, Switch spec 12.6).
fn note_peer(
    cx: &mut ReceiveContext<'_>,
    pubky: &[u8; 32],
    stored: &StoredMessage,
) -> Result<(), Fail> {
    let (peer, at) = peer_note(pubky, stored);
    keep_local(cx, local(&peer, &at)?);
    Ok(())
}

/// The note of `pubky` as a counterparty with Paykit activity (`peer/`), for re-sends after
/// a `missed` item or a provider loss (Paykit spec 9, Switch spec 12.6): its key, and the
/// acceptance time of `stored`, the message that shows it. A request or a Private Payment
/// List notes its sender, at its first read and at a stored entry's later write alike.
pub(crate) fn peer_note(pubky: &[u8; 32], stored: &StoredMessage) -> (String, u64) {
    (key::peer(pubky), stored.accepted_at().unwrap_or(0))
}

/// The value of the record the app holds at `target`, if it holds a readable one.
fn held_value(cx: &ReceiveContext<'_>, target: &ActivityTarget) -> Result<Option<Vec<u8>>, Fail> {
    Ok(cx
        .record(&target.record_target())?
        .and_then(|held| held.content.value().map(<[u8]>::to_vec)))
}

/// Hold the entry while the record at `target` holds a value not open only for want of its
/// scope key (Switch spec 3.5), as the client holds one whose write it can't seal yet: such
/// a value opens once the key comes, so it is neither absent nor a value that doesn't read,
/// and a message about it is read again then. Written over now, it would lose what it holds
/// unseen. The app may hold the current epoch's key and not the one the value is sealed at,
/// as with a bundle that came in parts, so the context's own wait doesn't cover it
/// ([`Fail::AwaitsKey`]).
fn wait_for_key(cx: &ReceiveContext<'_>, target: &ActivityTarget) -> Result<(), Fail> {
    let held = cx.record(&target.record_target())?;
    if let Some(RecordContent::Unreadable(Unopened::KeyNotHeld { data_epoch })) =
        held.map(|held| held.content)
    {
        return Err(Fail::AwaitsKey { data_epoch });
    }
    Ok(())
}

/// Whether the app's copy holds the record at `target` ([`holds_record`]).
fn held_request(cx: &ReceiveContext<'_>, target: &ActivityTarget) -> Result<bool, Fail> {
    Ok(holds_record(
        cx.record(&target.record_target())?
            .map(|held| held.content)
            .as_ref(),
    ))
}

/// Whether `content`, what the app's copy holds at a record, is a record held, for the stop a
/// payee's cancellation keeps: any content but a delete, a value that doesn't read and one
/// sealed under a scope key the app doesn't hold yet included. A stop for a request held
/// stays until it is placed; one for a request not held is bounded ([`keep_cancellation`]).
pub(crate) fn holds_record(content: Option<&RecordContent>) -> bool {
    content.is_some_and(|content| *content != RecordContent::Deleted)
}

/// The value held at `target`, which is of `side`: `None` for none, or one that isn't
/// open or is deleted. The entry is held instead while the value waits for its scope key
/// ([`wait_for_key`]), and while the record is absent or deleted and the app's copy of
/// `side` may lack records ([`hold_while_unlisted`]): the copy decides nothing then, a
/// delete it holds included, which a later write at the switch may have replaced.
fn held_or_unlisted(
    cx: &ReceiveContext<'_>,
    target: &ActivityTarget,
    side: RecordSide,
) -> Result<Option<Vec<u8>>, Fail> {
    wait_for_key(cx, target)?;
    let record = cx.record(&target.record_target())?;
    if !holds_record(record.as_ref().map(|held| &held.content)) {
        hold_while_unlisted(cx, side)?;
    }
    Ok(record.and_then(|held| held.content.value().map(<[u8]>::to_vec)))
}

/// Hold the entry while the app's copy of `side`'s records may lack records (Paykit spec
/// 8.1, "Records not listed yet"): after a restore, a `missed` item, or under another
/// certificate than the one they were listed under, until Paykit has listed them. The rule
/// is [`crate::hold::side_unlisted`], read per entry from the context's certificate and
/// Paykit's state (`nl`, `ml`, `lc`), so an entry after a certificate or a `missed` item
/// the same read took is held too. The entry is held as for a store unavailable for now
/// (Paykit spec 17, question 27), and read again at a later read.
fn hold_while_unlisted(cx: &ReceiveContext<'_>, side: RecordSide) -> Result<(), Fail> {
    if side_may_lack(cx, side)? {
        return Err(Fail::Store(StoreFailure::unavailable(
            StoreError::Unavailable,
        )));
    }
    Ok(())
}

/// Whether the app's copy of `side`'s records may lack records ([`hold_while_unlisted`]).
fn side_may_lack(cx: &ReceiveContext<'_>, side: RecordSide) -> Result<bool, Fail> {
    let marked = state::cx_get::<bool>(cx, key::RECORDS_UNLISTED)?.is_some()
        || state::cx_get::<bool>(cx, key::MISSED_UNLISTED)?.is_some();
    let listed = match state::cx_get::<SignedDoc>(cx, key::LISTED_CERT)? {
        None => None,
        Some(signed) => {
            Some(AppCert::verify(&signed, &cx.identity()).map_err(|_| Fail::corrupt())?)
        }
    };
    Ok(crate::hold::side_unlisted(
        side,
        marked,
        listed.as_ref(),
        cx.cert(),
    ))
}

/// Place `stored` in its `role` of the record at `target`, in the effect's batch, built on
/// the version the app holds. A record that doesn't read is written again with the message
/// (the reader doesn't act on an invalid record, Paykit wire 5.3). Nothing is written when
/// the record already reflects the message.
fn place(
    cx: &mut ReceiveContext<'_>,
    target: &ActivityTarget,
    counterparty: [u8; 32],
    role: Role,
    stored: &StoredMessage,
) -> Result<(), Fail> {
    wait_for_key(cx, target)?;
    // A record the copy holds deleted, at an app that saw its messages: what it saw stays
    // seen (Switch spec 8.2), so a message that may open it is one for the record it saw,
    // taken with nothing written: a request sent again changes nothing, and a proof of a
    // settlement it saw credited isn't credited again (Paykit spec 8.1, "Records not listed
    // yet"). Only an app holding no seen state of it opens it again.
    let record_target = target.record_target();
    if cx
        .record(&record_target)?
        .is_some_and(|held| held.content == RecordContent::Deleted)
        && cx.record_seen(&cx.record_key(&record_target)?)?.is_some()
    {
        return Ok(());
    }
    let own = cx.identity();
    let side = target.side.side();
    let current = held_value(cx, target)?;
    let mut record = activity::read(current.as_deref(), &own, side)
        .unwrap_or_else(|| activity::empty(side, counterparty));
    if !activity::place(&mut record, role, stored.clone()) {
        return Ok(());
    }
    cx.put_record(&target.record_target(), &record.to_json()?, Vec::new())?;
    Ok(())
}

/// A payee's cancellation of a one-time request, kept with the cursor move (Paykit spec
/// 8.1, the kept-copy stop): the receive step writes no record for it. The app places it in
/// the request record as a later write of its own ([`crate::Paykit::receive`],
/// [`crate::Paykit::resume_payments`] and before any payment), once it holds the record, a
/// request first unopenable and re-sent later (Paykit spec 9) included; and every new
/// attempt reads the kept copy itself, so a stop that can't be placed still refuses the
/// payment. A later cancellation of the same request adds nothing; one record holds one per
/// role (Paykit spec 6).
///
/// Security: the cancellation passed every recipient check and check 10 before it was kept,
/// and is verified again when it is placed. It is keyed by the payer's record of the request
/// it names first in `refs`, which derives from the sender and that hash, so it can only
/// cancel that sender's request. A stop for a request `held` ([`holds_record`]) is kept until
/// it is placed. One for a request not held is new state, which admission gates (Switch spec
/// 7), and is kept under the bounds of a payer's reply before its record, counted apart from
/// those: past its sender's [`MAX_KEPT_REPLIES_PER_SENDER`] such stops, that sender's earliest
/// goes, and past [`MAX_KEPT_REPLIES`] in all, the earliest of the sender keeping the most
/// ([`kept_reply_evicted`]). It goes [`KEPT_MESSAGE_MS`] after the cancellation's acceptance
/// while the request is still not held ([`crate::Paykit::place_kept_cancellations`]): by then
/// its sender re-sends nothing, and any own app that held the request got the cancellation
/// too.
fn keep_cancellation(
    cx: &mut ReceiveContext<'_>,
    target: &ActivityTarget,
    stored: &StoredMessage,
    held: bool,
) -> Result<(), Fail> {
    let kept = key::kept_cancellation(&cx.record_key(&target.record_target())?);
    if state::cx_get::<KeptMessage>(cx, &kept)?.is_some() {
        return Ok(());
    }
    if !held {
        make_room_for_stop(cx, &stored.sender())?;
    }
    keep_local(cx, local(&kept, &KeptMessage::of(stored))?);
    Ok(())
}

/// Make room for a new stop from `sender` among the stops kept for requests not held, in the
/// step: those the bounds drop ([`kept_reply_evicted`]) go, until there is room. Left out of
/// the bounds, neither counted nor dropped, are the stops for a request the copy holds
/// ([`holds_record`]) and those for a request the app keeps a stored message of to write
/// ([`crate::stored::writes_request`]), the request's own entry among them. These last are
/// bounded by the entries the client stores (Switch spec 8.2), one stop per request record;
/// counted here, they would make the stops of other requests go first. One that is left
/// out no longer counts again from then on, which can put its sender past the bound: the
/// loop drops back to it. It is reached only once the copy of the payer's records lacks none:
/// a cancellation of a request absent or deleted from a copy that may lack records is held
/// before ([`hold_while_unlisted`]), so every stop is judged from a listed copy.
fn make_room_for_stop(cx: &mut ReceiveContext<'_>, sender: &[u8; 32]) -> Result<(), Fail> {
    let mut kept = Vec::new();
    let mut names = Vec::new();
    for (name, stop) in state::cx_scan::<KeptMessage>(cx, key::KEPT_CANCELLATIONS)? {
        let Some(crate::state::StateKey::KeptCancellation(record)) =
            crate::state::StateKey::parse(name.as_bytes())
        else {
            return Err(Fail::corrupt());
        };
        let (cancellation, at) = kept_with_time(cx, &stop)?;
        let content = cx.record_at(&record)?.map(|held| held.content);
        if holds_record(content.as_ref()) {
            continue;
        }
        let from = cancellation.sender();
        if let Some(request) = cancellation.message().refs.first() {
            if keeps_stored_of(cx, &from, request)? {
                continue;
            }
        }
        kept.push((from, at));
        names.push(name.into_bytes());
    }
    while let Some(index) = kept_reply_evicted(&kept, sender) {
        kept.swap_remove(index);
        keep_local(
            cx,
            LocalWrite::Delete {
                key: names.swap_remove(index),
            },
        );
    }
    Ok(())
}

/// The message `kept`, read bound to its key (`state::Stored`), with its acceptance time,
/// which every stop and kept reply carries.
fn kept_with_time(
    cx: &ReceiveContext<'_>,
    kept: &KeptMessage,
) -> Result<(StoredMessage, u64), Fail> {
    let message = kept.stored(&cx.identity()).map_err(|_| Fail::corrupt())?;
    let at = message.accepted_at().ok_or_else(Fail::corrupt)?;
    Ok((message, at))
}

/// Whether the app keeps a stored message from `sender` of `request` still to write
/// ([`crate::stored::writes_request`]).
fn keeps_stored_of(
    cx: &ReceiveContext<'_>,
    sender: &[u8; 32],
    request: &[u8; 32],
) -> Result<bool, Fail> {
    let own = cx.identity();
    for (_, note) in state::cx_scan::<crate::stored::StoredNote>(cx, &key::stored_notes_of(sender))?
    {
        if crate::stored::writes_request(&note, &own, request).map_err(|_| Fail::corrupt())? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A payer's acceptance, rejection, cancellation or proof, at the payee (Paykit spec 3.4,
/// 6, 8.4). Returns whether it is about a record of this app's: a request it holds, or a
/// payment without a request.
fn payer_reply(
    cx: &mut ReceiveContext<'_>,
    stored: &StoredMessage,
    scopes: &[String],
    settlement: Settlement<'_>,
) -> Result<bool, Fail> {
    let sender = stored.sender();
    let own = cx.identity();
    let label = stored.body().label().cloned();
    // An app writes only records its scopes cover: a labelled selling app its own label's
    // (Paykit spec 3.1, 6).
    if !writes_payee_label(scopes, label.as_ref()) {
        return Ok(false);
    }
    let role = match stored.body() {
        Body::PaymentProof(_) => Role::Proof,
        Body::PaymentRequestCancellation(_) => Role::Cancellation,
        _ => Role::Response,
    };
    match stored.message().refs.first() {
        Some(request) => {
            let target =
                ActivityTarget::payee(natural_key::request_record(&own, request), label.as_ref());
            // A reply to a request this app holds no record of yet, such as one another own
            // app sent whose record this app hasn't taken yet, is kept until the record is
            // held, then placed (`Paykit::place_kept_replies`). It is new state, which the
            // spam rule gates, within the bounds on kept replies ([`kept_reply_evicted`]).
            // While the copy of the payee's records may lack records, the absence decides
            // nothing: the entry is held instead ([`held_or_unlisted`]).
            if held_or_unlisted(cx, &target, RecordSide::Payee)?.is_none() {
                if admits_new_state(cx, &sender, scopes)? {
                    keep_reply(cx, &target, stored)?;
                }
                return Ok(false);
            }
            place(cx, &target, sender, role, stored)?;
            Ok(true)
        }
        None => {
            // A payment without a request: keyed by its settlement, so every proof of one
            // settlement names one record and is credited once (Paykit spec 8.4). The app's
            // rail gives the settlement ID, so a method of its own is credited too; a proof
            // whose method gives none isn't credited.
            let Body::PaymentProof(proof) = stored.body() else {
                return Ok(false);
            };
            let Some(settlement) = settlement(&proof.proof) else {
                return Ok(false);
            };
            let target = ActivityTarget::payee(
                natural_key::payee_payment_record(&proof.payment_endpoint_identifier, &settlement),
                None,
            );
            // A proof no held record matches is new state, which the spam rule gates.
            let held = held_or_unlisted(cx, &target, RecordSide::Payee)?;
            if held.is_some() || admits_new_state(cx, &sender, scopes)? {
                place(cx, &target, sender, role, stored)?;
            }
            Ok(true)
        }
    }
}

/// Keep `stored`, a payer's reply to the request whose payee record is at `target`, which
/// the app doesn't hold yet (`pr/<sender>/<record key>/<msg_id>`), within the bounds on kept
/// replies ([`make_room_for_reply`]); one kept already is read with a restore's check and
/// left as it is. Admission is the caller's.
fn keep_reply(
    cx: &mut ReceiveContext<'_>,
    target: &ActivityTarget,
    stored: &StoredMessage,
) -> Result<(), Fail> {
    let sender = stored.sender();
    let record = cx.record_key(&target.record_target())?;
    let kept = key::kept_reply(&sender, &record, &stored.message().msg_id);
    // One kept already is read bound to its key, and stays as it is.
    if state::cx_get::<KeptMessage>(cx, &kept)?.is_none() {
        make_room_for_reply(cx, &sender)?;
        keep_local(cx, local(&kept, &KeptMessage::of(stored))?);
    }
    Ok(())
}

/// A receipt (Paykit spec 7.3, 8.4): stored in the payer's record and, in the same batch,
/// what [`receipt_settles`] decides: the attempt it names settled by evidence when it is
/// open, unknown or failed and the claim isn't SETTLED (Switch spec 10.2 row 3), or, once
/// the claim is gone, the notice "marked failed, was paid" for a payment made after
/// `failed` was written (Paykit spec 7.2). A stored entry's receipt written later
/// (`Paykit::place_receipt`) decides the same.
fn receipt_effect(
    cx: &mut ReceiveContext<'_>,
    stored: &StoredMessage,
    attempt_id: AttemptId,
) -> Result<(), Fail> {
    let sender = stored.sender();
    let own = cx.identity();
    let target = receipt_target(stored, &own, &attempt_id);
    let Some(value) = held_or_unlisted(cx, &target, RecordSide::Payer)? else {
        return Ok(());
    };
    let Some(record) = activity::read(Some(&value), &own, target.side.side()) else {
        return Ok(());
    };
    let Some(found) = receipt_on_record(&record, stored) else {
        return Ok(());
    };
    if found.names_held_proof {
        place(cx, &target, sender, Role::Receipt, stored)?;
    }
    // The journal entry of the attempt, if this app made it: it holds a payment without a
    // request's deadline, and whether `failed` stands on the claim of a payment made. One
    // that doesn't read parks the receipt rather than settling the claim with no deadline,
    // or consuming it (Switch spec 8.2): the store is corrupt. It is read as
    // `Paykit::journal_entry_on` reads it: bound to its key, and to the claim the receipt
    // settles, so an entry of the attempt on another claim parks the receipt too.
    let claim = receipt_claim(stored, &attempt_id, |request| {
        obligation(cx, &sender, request, 0)
    });
    let journaled = JournalEntry::on_claim(
        state::cx_get::<JournalEntry>(cx, &key::journal(&attempt_id))?,
        &claim,
    )
    .map_err(|_| Fail::corrupt())?;
    // A receipt reaches only an app holding `pay`, which carries `claims.write`, and an entry
    // that comes under a certificate without it is held unprocessed: this returns only for a
    // certificate Paykit's checks would refuse.
    if !cx.cert().has_permission(Permission::ClaimsWrite) {
        return Ok(());
    }
    let obligation = claim;
    let deadline = receipt_deadline(stored, &record, journaled.as_ref());
    // A claim the app keeps no view of is read from the switch first, so the receipt
    // settles it in the cursor's batch (Switch spec 10.2 row 3).
    let read = cx.switch_claim(&obligation).map(|fetch| match fetch {
        ClaimFetch::Read(read) => SwitchClaim::Read(Some(read.view.clone())),
        ClaimFetch::Absent => SwitchClaim::Read(None),
        // Refused, such as without `claims.read`.
        _ => SwitchClaim::Refused,
    });
    let view = match failure_view(cx.claim_view(&obligation)?, read) {
        FailureView::View(view) => view,
        FailureView::Refused => return Ok(()),
        FailureView::ReadSwitch => return Err(Fail::Need(vec![Need::Claim(obligation)])),
    };
    let settles = receipt_settles(&ReceiptClaim {
        stored,
        attempt_id,
        obligation,
        deadline,
        view: view.as_ref(),
        service_time: cx.highest_service_time(),
        journaled,
        first_receipt: found.first_receipt,
    })?;
    if let Some((op, deadline)) = settles.claim {
        cx.claim(op, Vec::new(), deadline)?;
    }
    if let Some(late) = settles.notice {
        let notice = key::notice(&attempt_id, NoticeKind::PaidAfterFailed);
        // A notice kept already is read whole and bound to its key, as `Paykit::kept_notice`
        // reads it.
        if state::cx_get::<KeptNotice>(cx, &notice)?.is_none() {
            keep_local(cx, local(&notice, &late)?);
        }
    }
    if let Some(entry) = settles.received {
        keep_local(cx, local(&key::journal(&attempt_id), &entry)?);
    }
    Ok(())
}

/// What the switch answered for a receipt's claim (Switch wire 7.4.8).
pub(crate) enum SwitchClaim {
    /// The claim as read, `None` when the switch holds none.
    Read(Option<ClaimView>),
    /// The read refused for good, such as without `claims.read`.
    Refused,
}

/// The claim view a receipt decides on ([`failure_view`]).
#[derive(Debug)]
pub(crate) enum FailureView {
    /// The view, `None` when the switch holds no claim and the app keeps none.
    View(Option<ClaimView>),
    /// The switch refused its read: the receipt settles nothing, and is still recorded.
    Refused,
    /// The app keeps no view: the switch's answer is needed first.
    ReadSwitch,
}

/// The claim view a receipt's claim effect decides on ([`receipt_settles`]), the one rule
/// for its first read (`receipt_effect`) and a stored entry's later write
/// (`Paykit::place_receipt`): the view the app keeps, `kept`, which holds the failure after
/// the service deletes the claim, or, only when it keeps none, the switch's answer, `read`,
/// which the caller fetches once this asks for it.
pub(crate) fn failure_view(kept: Option<ClaimView>, read: Option<SwitchClaim>) -> FailureView {
    match (kept, read) {
        (Some(view), _) => FailureView::View(Some(view)),
        (None, Some(SwitchClaim::Read(view))) => FailureView::View(view),
        (None, Some(SwitchClaim::Refused)) => FailureView::Refused,
        (None, None) => FailureView::ReadSwitch,
    }
}

/// The payer's record a receipt goes into: its request's, or for a payment without a
/// request the record of the attempt it names (Paykit wire 4.11, 5.3). This SDK makes no
/// payment of a Billing Period: the request record holds no proof of one, so a receipt
/// naming a period is neither stored nor settles its claim here.
pub(crate) fn receipt_target(
    stored: &StoredMessage,
    own: &[u8; 32],
    attempt_id: &AttemptId,
) -> ActivityTarget {
    match stored.message().refs.get(1) {
        Some(request) => {
            ActivityTarget::payer(natural_key::request_record(&stored.sender(), request))
        }
        None => ActivityTarget::payer(natural_key::payer_payment_record(own, attempt_id)),
    }
}

/// What a receipt finds in the payer's record it names ([`receipt_on_record`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReceiptOnRecord {
    /// The record holds the proof the receipt names, so the receipt goes beside it.
    pub(crate) names_held_proof: bool,
    /// The record holds the proof the receipt names and no receipt yet: this receipt is the
    /// first of that payment.
    pub(crate) first_receipt: bool,
}

/// What a receipt `stored` finds in `record`, the payer's record it names: `None` when the
/// record's counterparty isn't the receipt's sender, through whom the payer matches it
/// (Paykit spec 8.4), so the receipt does nothing.
///
/// A receipt names the proof it confirms first (Paykit wire 4.11), and the record holds a
/// receipt only beside that proof (Paykit wire 5.3): the payer's proof goes into the record
/// in the batch that sends it, so a receipt for a record without it, or naming another
/// proof, isn't stored. Stored, it would leave the payer unable to record its own proof. It
/// still settles the attempt it names. Only stored messages decide here: the record's
/// `status` is informational, rewritten by every writer, so it is never read (Paykit wire
/// 5.3).
pub(crate) fn receipt_on_record(
    record: &ActivityRecord,
    stored: &StoredMessage,
) -> Option<ReceiptOnRecord> {
    if record.counterparty != stored.sender() {
        return None;
    }
    let names_held_proof = record
        .proof
        .as_ref()
        .is_some_and(|proof| stored.message().refs.first() == Some(&proof.hash()));
    Some(ReceiptOnRecord {
        names_held_proof,
        first_receipt: names_held_proof && record.receipt.is_none(),
    })
}

/// The claim a receipt settles: its request's obligation, blinded by `request_obligation`;
/// or, for a payment without a request, the attempt's own obligation.
pub(crate) fn receipt_claim(
    stored: &StoredMessage,
    attempt_id: &AttemptId,
    request_obligation: impl FnOnce(&[u8; 32]) -> [u8; 32],
) -> [u8; 32] {
    match stored.message().refs.get(1) {
        Some(request) => request_obligation(request),
        None => paykit_lib::derive::payment_obligation(attempt_id),
    }
}

/// The deadline of a receipt's claim: its request's attempt deadline; or, for a payment
/// without a request, the deadline its journal entry kept, `journaled` read bound to the
/// claim ([`JournalEntry::on_claim`]).
pub(crate) fn receipt_deadline(
    stored: &StoredMessage,
    record: &ActivityRecord,
    journaled: Option<&JournalEntry>,
) -> ObligationDeadline {
    match stored.message().refs.get(1) {
        Some(_) => crate::requests::request_of(record)
            .and_then(|(_, body)| body.attempt_deadline(None).ok())
            .map_or(ObligationDeadline::NotKnown, crate::pay::own_deadline),
        None => journaled.map_or(ObligationDeadline::NotKnown, |entry| {
            crate::pay::own_deadline(entry.deadline)
        }),
    }
}

/// What [`receipt_settles`] decides from.
pub(crate) struct ReceiptClaim<'a> {
    pub(crate) stored: &'a StoredMessage,
    pub(crate) attempt_id: AttemptId,
    pub(crate) obligation: [u8; 32],
    pub(crate) deadline: ObligationDeadline,
    /// The claim as read now, `None` when the switch holds none.
    pub(crate) view: Option<&'a ClaimView>,
    /// The highest service time verified.
    pub(crate) service_time: Option<u64>,
    /// The attempt's journal entry, if this app holds it.
    pub(crate) journaled: Option<JournalEntry>,
    /// The receipt is the first of the payment its record holds the proof of.
    pub(crate) first_receipt: bool,
}

/// What a receipt does beside its placing ([`receipt_settles`]), each part in the batch that
/// places it.
#[derive(Debug, Default)]
pub(crate) struct ReceiptSettles {
    /// The attempt settled by evidence, and the obligation's deadline.
    pub(crate) claim: Option<(ClaimOp, ObligationDeadline)>,
    /// The notice "marked failed, was paid", kept unless one is held at its key already.
    pub(crate) notice: Option<KeptNotice>,
    /// The attempt's journal entry, kept as no longer waiting for a receipt.
    pub(crate) received: Option<JournalEntry>,
}

/// What a receipt does on its claim and in the journal, the same at its first read and at
/// a stored entry's later write (Paykit spec 7.2, 7.3, 8.1; Switch spec 10.2 row 3).
///
/// The claim is gone, or may be, when the switch holds none, or its pinned deadline is 30
/// days or more before the highest service time verified, when the switch may delete it
/// (Switch spec 6.3): the receipt can't be recorded on it. For a payment made after
/// `failed` was written on it, the user is told it was paid (Paykit spec 7.2, "What a
/// payment method declares"), and the entry no longer waits. An app that gets the receipt
/// and holds the failure tells the user: it holds the failure when it has it from a stable
/// source, the attempt's owner from its own journal (`failed_on_claim`), or any own app,
/// the owner included, from a view of the claim, kept or read, that shows the attempt
/// `failed`, for the first receipt of the proof the record holds. An owner restored from a
/// backup that predates the failure has no journal entry for it, and uses such a view. The record's `status` is informational and never
/// decides it (Paykit wire 5.3). An own app that never saw the failure, such as one that
/// joined after the service deleted the claim, can't tell and raises nothing. Either way
/// the receipt goes into the record, by the caller. A claim SETTLED holds the
/// payment, and one with the attempt open, unknown or failed is settled by this receipt's
/// evidence: either way an entry waiting for its receipt no longer waits.
///
/// # Errors
///
/// The client's refusal of the claim operation as built.
pub(crate) fn receipt_settles(claim: &ReceiptClaim<'_>) -> Result<ReceiptSettles, AppError> {
    let waiting = claim
        .journaled
        .clone()
        .filter(|entry| entry.failed_on_claim)
        .map(|mut entry| {
            entry.failed_on_claim = false;
            entry
        });
    let gone = claim.view.is_none_or(|view| {
        view.deadline.is_some_and(|deadline| {
            claim
                .service_time
                .is_some_and(|t| deadline.saturating_add(RECONCILIATION_MS) <= t)
        })
    });
    let mut out = ReceiptSettles::default();
    let Some(view) = claim.view.filter(|_| !gone) else {
        let failed_on_view = claim
            .view
            .and_then(|view| view.attempt(&claim.attempt_id.to_bytes()))
            .is_some_and(|attempt| attempt.outcome == Outcome::Failed);
        if waiting.is_some() || (claim.first_receipt && failed_on_view) {
            out.notice = Some(KeptNotice::late(
                claim.attempt_id,
                claim.stored.sender(),
                claim.stored.message().refs.get(1),
                claim.service_time.unwrap_or(0),
            ));
        }
        out.received = waiting;
        return Ok(out);
    };
    if view.status == ClaimStatus::Settled {
        out.received = waiting;
        return Ok(out);
    }
    let open = view.attempts.iter().any(|a| {
        a.attempt_id == claim.attempt_id.to_bytes()
            && matches!(
                a.outcome,
                Outcome::Open | Outcome::Unknown | Outcome::Failed
            )
    });
    if open {
        let op = ClaimOp::done(
            claim.obligation,
            view.step,
            claim.attempt_id.to_bytes(),
            Basis::Evidence,
            Some(claim.stored.hash()),
        )?;
        out.claim = Some((op, claim.deadline));
        out.received = waiting;
    }
    Ok(out)
}

/// The `status` of a payer's record whose payment was made after `failed` was written on its
/// claim, which stays `failed` until a receipt settles it (Paykit wire 5.3: informational,
/// written for display and never read for a decision).
pub(crate) const PAID_AFTER_FAILED: &str = "failed";

/// The obligation ID of a request or Billing Period, blinded through the receive context
/// (Paykit wire 1.5).
pub(crate) fn obligation(
    cx: &ReceiveContext<'_>,
    payee: &[u8; 32],
    request_hash: &[u8; 32],
    period_start_secs: u64,
) -> [u8; 32] {
    request_obligation_blinded(&cx.blind(payee), &cx.blind(request_hash), period_start_secs)
}

/// A Private Payment List (Paykit spec 5, 8.4; Paykit wire 5.4): the latest list of its
/// publishing app replaces an older one, keeping the single-use endpoints marked used.
///
/// A `payment_lists` record that doesn't read, such as a value a faulty own app wrote that
/// this app never saw read before (else the client refuses it as a regression and keeps
/// what it saw, Switch spec 8.2), or one sealed at an epoch whose key the app lacks, is
/// never written over: its single-use marks can't be seen, and a list written without them
/// would let an endpoint already paid be paid again. A record whose current write is a
/// delete is held the same way: the delete took the marks with it. The list is held
/// instead ([`key::held_list`], the newest version kept), a durable notice
/// ([`Paykit::held_lists`]), and placed once another own app that holds the marks writes
/// the record again ([`Paykit::place_held_lists`]), or when the host, having shown the user
/// that single-use endpoints may be paid twice, places it without them
/// ([`Paykit::place_held_list_without_marks`]). Payments meanwhile don't use the record:
/// lists that don't read, or are held, are never paid to. The SDK never deletes a record.
///
/// The limit: a delete is seen only by an app that holds the record's delete write, from
/// its feed or a read. To an app that never held the record, a deleted one looks absent,
/// and the switch takes a create over a delete, so its first list is written afresh. A
/// record merely absent is never held, or a payee's first list could never be stored.
fn list_effect(cx: &mut ReceiveContext<'_>, stored: &StoredMessage) -> Result<(), Fail> {
    let payee = stored.sender();
    let app_id = stored.message().from.app_id;
    if !matches!(stored.body(), Body::PrivatePaymentList(_)) {
        return Ok(());
    }
    let own = cx.identity();
    let target = list_target(&payee, &app_id);
    let held = match cx.record(&target)?.map(|r| r.content) {
        None => None,
        Some(RecordContent::Value(value)) => match PaymentListRecord::read(&value, &own) {
            Ok(held) => Some(held),
            Err(_) => return hold_list(cx, &target, stored),
        },
        Some(RecordContent::Deleted | RecordContent::Unreadable(_)) => {
            return hold_list(cx, &target, stored)
        }
    };
    if let Some(record) = next_list_record(held, stored) {
        cx.put_record(&target, &record.to_json()?, Vec::new())?;
    }
    Ok(())
}

/// The `payment_lists` record after `stored`, a list, on `held`; `None` when nothing
/// changes: the list is reflected already, or of a version no newer than the one held
/// (Paykit wire 4.13). The marks of single-use endpoints used are kept.
pub(crate) fn next_list_record(
    held: Option<PaymentListRecord>,
    stored: &StoredMessage,
) -> Option<PaymentListRecord> {
    let Body::PrivatePaymentList(list) = stored.body() else {
        return None;
    };
    let msg_id = &stored.message().msg_id;
    let mut record = match held {
        Some(held) => {
            if held.list.message().msg_id == *msg_id || held.msg_ids.contains(msg_id) {
                return None;
            }
            if list.version <= list_version(&held.list) {
                return None;
            }
            held
        }
        None => PaymentListRecord {
            payee: stored.sender(),
            app_id: stored.message().from.app_id,
            list: stored.clone(),
            used: Vec::new(),
            msg_ids: Vec::new(),
            unknown: Default::default(),
        },
    };
    record.list = stored.clone();
    record.msg_ids =
        paykit_lib::records::bounded_msg_ids(std::slice::from_ref(msg_id), &record.msg_ids);
    Some(record)
}

/// A list's version; 0 for a message that isn't one.
pub(crate) fn list_version(stored: &StoredMessage) -> u64 {
    match stored.body() {
        Body::PrivatePaymentList(list) => list.version,
        _ => 0,
    }
}

/// Hold `stored`, a list whose `payment_lists` record doesn't read, with the cursor move,
/// unless a newer list is held already ([`list_effect`]).
fn hold_list(
    cx: &mut ReceiveContext<'_>,
    target: &pubky_switch_client::app::RecordTarget,
    stored: &StoredMessage,
) -> Result<(), Fail> {
    let kept = key::held_list(&cx.record_key(target)?);
    if let Some(held) = state::cx_get::<KeptMessage>(cx, &kept)? {
        let held = held.stored(&cx.identity()).map_err(|_| Fail::corrupt())?;
        if list_version(&held) >= list_version(stored) {
            return Ok(());
        }
    }
    keep_local(cx, local(&kept, &KeptMessage::of(stored))?);
    Ok(())
}

/// How many payer's replies to requests this app holds no record of it keeps from one
/// sender identity, as Switch spec 8.2 bounds the entries of an unverified sender: past it,
/// that sender's earliest reply is dropped. The payees' cancellations kept as stops for
/// requests the app holds no record of are bounded the same way, counted apart.
pub const MAX_KEPT_REPLIES_PER_SENDER: usize = 64;

/// How many such replies it keeps in all: past it, the earliest reply of the sender holding
/// the most is dropped.
pub const MAX_KEPT_REPLIES: usize = 128;

/// A kept reply, as the bounds count it: its sender and its acceptance time.
pub(crate) type CountedReply = ([u8; 32], u64);

/// Which of `kept` goes for a new reply from `sender` to be kept (Switch spec 8.2's rule):
/// past the sender's [`MAX_KEPT_REPLIES_PER_SENDER`], its reply accepted earliest; past
/// [`MAX_KEPT_REPLIES`] in all, the one accepted earliest of the sender holding the most, a
/// tie going against the lower sender identity. `None` while there is room.
pub(crate) fn kept_reply_evicted(kept: &[CountedReply], sender: &[u8; 32]) -> Option<usize> {
    let earliest_of = |who: &[u8; 32]| {
        kept.iter()
            .enumerate()
            .filter(|(_, (from, _))| from == who)
            .min_by_key(|(_, (_, at))| *at)
            .map(|(index, _)| index)
    };
    let held = |who: &[u8; 32]| kept.iter().filter(|(from, _)| from == who).count();
    if held(sender) >= MAX_KEPT_REPLIES_PER_SENDER {
        return earliest_of(sender);
    }
    if kept.len() < MAX_KEPT_REPLIES {
        return None;
    }
    let most = kept
        .iter()
        .map(|(from, _)| *from)
        .max_by_key(|from| (held(from), std::cmp::Reverse(*from)))?;
    earliest_of(&most)
}

/// Make room for a new reply from `sender` among the kept replies, in the step: the one the
/// bounds drop ([`kept_reply_evicted`]) goes, and counts in its sender's mark
/// (`pd/<sender>`, `marks.rs`) with its acceptance time, so the sender is asked to send it
/// again once the record of one of its requests is held. It is dropped as one never
/// processed (Paykit spec 8.1, "Replies before their record"): the client marked it
/// processed when it was kept, so the step forgets its pair
/// ([`ReceiveContext::forget_processed`]), in the commit that drops it and writes the mark,
/// and a re-sent copy is taken as a new message. This is the one pair a Paykit App forgets
/// (Switch spec 11); a reply a later copy brings again is never applied twice, since a
/// record that reflects its `msg_id` isn't written again.
fn make_room_for_reply(cx: &mut ReceiveContext<'_>, sender: &[u8; 32]) -> Result<(), Fail> {
    let mut kept = Vec::new();
    let mut names = Vec::new();
    let mut msg_ids = Vec::new();
    for (name, reply) in state::cx_scan::<KeptMessage>(cx, key::KEPT_REPLIES)? {
        let (reply, at) = kept_with_time(cx, &reply)?;
        kept.push((reply.sender(), at));
        names.push(name.into_bytes());
        msg_ids.push(reply.message().msg_id.clone());
    }
    let Some(index) = kept_reply_evicted(&kept, sender) else {
        return Ok(());
    };
    let (from, at) = kept[index];
    match cx.forget_processed(from, &msg_ids[index]) {
        Ok(_) => {}
        // The client won't forget the pair now: an entry it stores locally, or an effect it
        // keeps to send, holds the message. Dropped without forgetting it, the reply could
        // never be taken again, so it stays kept, and the bounds are over by one for this
        // step.
        Err(AppError::Refused(ClientRefusal::NotNow(_))) => return Ok(()),
        // Past the step's bound on local writes: the context marks the step, and the
        // client stores the entry as too large, applying none of it (`keep_local`).
        Err(AppError::Refused(ClientRefusal::Usage(_))) => {}
        Err(error) => return Err(error.into()),
    }
    keep_local(
        cx,
        LocalWrite::Delete {
            key: names.swap_remove(index),
        },
    );
    let family = MarkFamily::Replies;
    let mark_key = family.key(&from);
    let held = state::cx_get::<crate::marks::DropMark>(cx, &mark_key)?;
    let mut others = Vec::new();
    if held.is_none() {
        for (_, mark) in state::cx_scan::<crate::marks::DropMark>(cx, family.prefix())? {
            others.push((mark.sender, mark.latest_ms));
        }
    }
    let counted = count_dropped(held, &others, from, at, false);
    if let Some(gone) = counted.evicted {
        keep_local(
            cx,
            LocalWrite::Delete {
                key: family.key(&gone).into_bytes(),
            },
        );
    }
    if let Some(mark) = counted.put {
        keep_local(cx, local(&mark_key, &mark)?);
    }
    Ok(())
}

/// How long a message kept to be written into its record later is tried: 30 days from its
/// acceptance, the time its sender keeps it to send again (Paykit spec 9). While its record
/// decides nothing yet, it waits without this limit and is never given up: the record's
/// value is sealed at a key the app doesn't hold yet (`Unopened::KeyNotHeld`), the app
/// lacks the current data epoch's key of the record's scope, or the record is absent while
/// the copy of its side may lack records (Paykit spec 8.1, "Records not listed yet").
pub const KEPT_MESSAGE_MS: u64 = 30 * pubky_switch_core::records::DAY_MS;

/// How long an app holding `contacts` goes between its reads of the lists, the flagged
/// senders among them (Paykit wire 4.1: at most every 10 minutes).
pub const LISTS_REFRESH_MS: u64 = 10 * 60_000;

/// A Private Payment List held because its `payment_lists` record doesn't read, or is
/// deleted ([`Paykit::held_lists`]): never written over it, which could drop the single-use
/// marks it holds or held, until the record reads again or the host places it without them
/// ([`Paykit::place_held_list_without_marks`]). No payment uses that payee app's list
/// meanwhile.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HeldList {
    /// The payee.
    pub payee: [u8; 32],
    /// The payee's app that published the list.
    pub app_id: [u8; 16],
    /// The list's version.
    pub version: u64,
    /// The list message's `msg_id`.
    pub msg_id: String,
}

impl Paykit {
    /// The Private Payment Lists held because their `payment_lists` record doesn't read or
    /// is deleted (Paykit wire 5.4): a durable notice for the host. Each is placed once the
    /// record reads again, at a receive, or by
    /// [`Paykit::place_held_list_without_marks`]; until then no payment uses the payee app's
    /// list. A list held over a value only waiting for its scope key isn't listed: that
    /// value opens once the key comes, and the list is placed with its marks then, so
    /// nothing offers to place it without them.
    ///
    /// # Errors
    ///
    /// A store failure, or a held list that doesn't read.
    pub async fn held_lists(&self) -> crate::error::Result<Vec<HeldList>> {
        let own = self.identity();
        let mut out = Vec::new();
        for (rest, kept) in self.local_scan::<KeptMessage>(key::HELD_LISTS).await? {
            let record_key = pubky_switch_core::enc::unb64_array::<32>(&rest)
                .map_err(|_| crate::error::PaykitError::corrupt())?;
            if self
                .app
                .shared_record(&record_key)
                .await?
                .is_some_and(|r| crate::hold::awaits_key(&r.content))
            {
                continue;
            }
            let list = kept.stored(&own)?;
            out.push(HeldList {
                payee: list.sender(),
                app_id: list.message().from.app_id,
                version: list_version(&list),
                msg_id: list.message().msg_id.clone(),
            });
        }
        Ok(out)
    }

    /// Place the lists held because their record didn't read or was deleted, once another
    /// own app has written it again so it reads (Paykit wire 5.4): the record's update,
    /// built on the version held with its marks kept, and the notice's delete, in one batch.
    /// A record that still doesn't read, or is still deleted, keeps its list held, as does
    /// one absent while the payer's side isn't listed yet; a list no newer than the
    /// record's is dropped. A placement that fails now is tried again at the
    /// next receive, with no bound in time: one held list per payee app, replaced by a
    /// newer one, and one write of it per receive.
    pub(crate) async fn place_held_lists(&mut self) -> crate::error::Result<()> {
        for (rest, kept) in self.local_scan::<KeptMessage>(key::HELD_LISTS).await? {
            // One that can't be placed now stays held, for the next receive; the others are
            // still placed, and the receive goes on. A held list that doesn't read or isn't
            // at its own record's key is a corrupt store, which is surfaced.
            match self.place_held_list(&rest, &kept).await {
                Err(error) if error == crate::error::PaykitError::corrupt() => return Err(error),
                _ => {}
            }
        }
        Ok(())
    }

    /// Place one held list (`lh/<rest>`), as [`Paykit::place_held_lists`] says.
    async fn place_held_list(
        &mut self,
        rest: &str,
        kept: &KeptMessage,
    ) -> crate::error::Result<()> {
        let own = self.identity();
        let list = kept.stored(&own)?;
        let target = list_target(&list.sender(), &list.message().from.app_id);
        let record_key = self.app.record_key(&target)?;
        if format!("{}{rest}", key::HELD_LISTS) != key::held_list(&record_key) {
            return Err(crate::error::PaykitError::corrupt());
        }
        let held = match self
            .app
            .shared_record(&record_key)
            .await?
            .map(|r| r.content)
        {
            // Absent while the copy of the payer's side may lack records, as after a join
            // or a restore before the first listing: the absence decides nothing yet, so the
            // list stays held (Paykit spec 8.1, "Records not listed yet").
            None if self.side_unlisted(RecordSide::Payer).await? => return Ok(()),
            None => None,
            Some(RecordContent::Value(value)) => match PaymentListRecord::read(&value, &own) {
                Ok(held) => Some(held),
                Err(_) => return Ok(()),
            },
            Some(RecordContent::Deleted | RecordContent::Unreadable(_)) => return Ok(()),
        };
        let mut tx = Tx::default();
        tx.delete(&key::held_list(&record_key));
        match next_list_record(held, &list) {
            Some(record) => {
                let value = record.to_json()?;
                let op = self
                    .app
                    .record_op(&target, Some(&value), Vec::new())
                    .await?;
                let batch = pubky_switch_client::app::OpBatch::new()
                    .entry(op)
                    .locals(tx.into_writes());
                let _ = self.app.submit_batch(batch).await;
            }
            None => self.local_commit(tx).await?,
        }
        Ok(())
    }

    /// Place the payer's replies kept for requests this app held no record of, each once
    /// the record is held, as a later write of its own with the kept copy dropped in its
    /// batch (Switch spec 8.2, Paykit spec 8.1). A placement that fails now is tried again
    /// at the next receive. Once [`KEPT_MESSAGE_MS`] have passed since a reply's acceptance,
    /// on the highest service time verified:
    /// - with its record absent or deleted, it is dropped, and stays processed: the payer
    ///   re-sends nothing first accepted that long ago (Paykit spec 9), so nothing asks for
    ///   it again, unlike a reply dropped for the bounds on kept replies, whose pair is
    ///   forgotten (`make_room_for_reply`). Never while the copy of the payee's records may
    ///   lack records ([`Paykit::side_unlisted`]): the absence decides nothing then;
    /// - with its record held but not open, sealed under a scope key the app doesn't hold
    ///   or a value that doesn't read, or with a sender that isn't the record's
    ///   counterparty, it is given up, and the host told
    ///   ([`crate::NoticeKind::MessageGivenUp`]) in the commit that drops it, as a stored
    ///   entry's message (`stored.rs`).
    ///
    /// A reply over a held record goes through the stored entries' later write
    /// ([`Paykit::place_note`]) and its one rule ([`crate::stored::later_write`]): a write the
    /// switch refuses, for good or over a quota, past the window gives the reply up with the
    /// notice; a write that can't be built or sent now, for a store failure or a switch that
    /// doesn't answer, is an error, returned, and the reply stays kept. Every kept reply is
    /// read with a restore's check, and one that fails it fails the call as a corrupt store.
    pub(crate) async fn place_kept_replies(&mut self) -> crate::error::Result<()> {
        let own = self.identity();
        for (rest, kept) in self.local_scan::<KeptMessage>(key::KEPT_REPLIES).await? {
            // Read bound to its key, as a restore reads it: a reply that fails it is a
            // corrupt store, so the receive fails rather than drop, give up or place it.
            let reply = kept.stored(&own)?;
            let kept_key = format!("{}{rest}", key::KEPT_REPLIES);
            let (Some(request), Some(role), Some(accepted_at)) = (
                reply.message().refs.first(),
                crate::activity::Role::of(reply.body()),
                reply.accepted_at(),
            ) else {
                return Err(crate::error::PaykitError::corrupt());
            };
            let target = ActivityTarget::payee(
                natural_key::request_record(&own, request),
                reply.body().label(),
            );
            let content = self
                .app
                .shared_record(&self.activity_key(&target)?)
                .await?
                .map(|held| held.content);
            // No record yet: the reply waits for it within its window, and past it is dropped
            // with no notice, its sender keeping it no longer either, but never while the
            // copy may lack records.
            if matches!(content, None | Some(RecordContent::Deleted)) {
                if crate::stored::past_window(accepted_at, self.service_time())
                    && !self.side_unlisted(RecordSide::Payee).await?
                {
                    self.local_delete(&kept_key).await?;
                }
                continue;
            }
            let record = match content {
                Some(RecordContent::Value(_)) => self.held_activity(&target).await?,
                _ => None,
            };
            // A reply the record can't take, its sender not the record's counterparty, isn't
            // written: kept no longer than any other.
            let placed = match record {
                Some(record) if record.counterparty != reply.sender() => crate::stored::Placed::Not,
                _ => {
                    let mut tx = Tx::default();
                    tx.delete(&kept_key);
                    self.place_note(&target, role, false, reply.clone(), tx)
                        .await?
                }
            };
            if crate::stored::later_write(placed, accepted_at, self.service_time())
                == crate::stored::LaterWrite::GiveUp
            {
                self.give_up(&kept_key, &reply).await?;
            }
        }
        Ok(())
    }

    /// How many messages from `sender` the spam rule only counted (Paykit wire 4.1): those
    /// that would have created state while the user had blocked it or the service flagged
    /// it.
    ///
    /// # Errors
    ///
    /// A store failure, or a kept count that doesn't read.
    pub async fn spam_count(&self, sender: &[u8; 32]) -> crate::error::Result<u64> {
        Ok(self
            .local_get::<u64>(&key::spam(sender))
            .await?
            .unwrap_or(0))
    }

    /// Read the lists again (`GET /allow`, [`pubky_switch_client::app::App::allow_list`])
    /// when the last read is [`LISTS_REFRESH_MS`] old or more on the host's clock, or none
    /// is kept, at an app holding `contacts`, so the flagged senders the spam rule reads stay
    /// fresh (Paykit wire 4.1). A clock that went back reads as due.
    ///
    /// It fails closed: a refused or failed read drops the time of the last one
    /// ([`key::LISTS_READ`]), so the copy of the lists isn't known to be complete, and the
    /// receive step defers every message that would create state until a read succeeds, at
    /// a later receive. Updates to held records still apply meanwhile. A store failure or an
    /// alarm is an error.
    pub(crate) async fn refresh_lists_when_due(&mut self) -> crate::error::Result<()> {
        // No Paykit action under a certificate Paykit refuses, a read made as the app and a
        // local write included (Paykit spec 7.2, "The restored certificate").
        if self.certificate_refused().is_some() || !reads_lists(self.app.cert().scopes()) {
            return Ok(());
        }
        let now = self.now();
        let last = self.local_get::<u64>(key::LISTS_READ).await?;
        if last.is_some_and(|last| last <= now && now - last < LISTS_REFRESH_MS) {
            return Ok(());
        }
        match self.app.allow_list().await {
            Ok(lists) => {
                let (deny, mut tx) = self.judged_deny(lists.deny).await?;
                tx.put(key::LISTS_READ, &now)?.put(key::DENIED, &deny)?;
                self.local_commit(tx).await
            }
            Err(error @ (AppError::Alarm(_) | AppError::Store(_))) => Err(error.into()),
            Err(_) if last.is_some() => self.local_delete(key::LISTS_READ).await,
            Err(_) => Ok(()),
        }
    }

    /// Whether the identity's deny list holds `pubky` (Paykit spec 8.1, "Blocked
    /// counterparties"), read from the switch now (`GET /allow`); the copy kept for the views
    /// is replaced with it. Every paying app holds `contacts`, which reads the lists (Paykit
    /// spec 3.1).
    ///
    /// # Errors
    ///
    /// [`crate::Refusal::SwitchUnavailable`] when the own switch doesn't answer; the
    /// client's errors: a read that fails is never taken as a payee not blocked.
    pub(crate) async fn denied(&mut self, pubky: &[u8; 32]) -> crate::error::Result<bool> {
        let lists = self.app.allow_list().await.map_err(|error| {
            if crate::pay::switch_unavailable(&error) {
                crate::error::Refusal::SwitchUnavailable(error.to_string()).into()
            } else {
                crate::error::PaykitError::from(error)
            }
        })?;
        let (deny, mut tx) = self.judged_deny(lists.deny).await?;
        tx.put(key::LISTS_READ, &self.now())?
            .put(key::DENIED, &deny)?;
        self.local_commit(tx).await?;
        Ok(deny.contains(pubky))
    }

    /// The deny list as this app judges payments by, from `deny` as read from the switch:
    /// with this app's own blocks and unblocks whose batch the client keeps unanswered
    /// (`bk/`), which count at once (Paykit spec 8.1, "a block or unblock the app wrote
    /// counts at once"): a kept `deny.put` adds its counterparty, a kept `deny.delete` takes
    /// it out. The marks the list shows landed go, in the returned commit.
    async fn judged_deny(
        &self,
        mut deny: Vec<[u8; 32]>,
    ) -> crate::error::Result<(Vec<[u8; 32]>, Tx)> {
        let mut tx = Tx::default();
        for (rest, blocked) in self.local_scan::<bool>(key::OWN_BLOCKS).await? {
            let pubky = pubky_switch_core::enc::unb64_array::<32>(&rest)
                .map_err(|_| crate::error::PaykitError::corrupt())?;
            let listed = deny.contains(&pubky);
            if listed == blocked {
                tx.delete(&key::own_block(&pubky));
            } else if blocked {
                deny.push(pubky);
            } else {
                deny.retain(|other| *other != pubky);
            }
        }
        Ok((deny, tx))
    }

    /// Whether the identity's deny list holds `pubky`, for a payment the user starts (Paykit
    /// spec 8.1): as read at most [`LISTS_REFRESH_MS`] ago, on the device clock, so the
    /// payment makes no call of its own for it; read first ([`Paykit::denied`]) when the copy
    /// is older or missing, and a read that fails then never counts as not blocked.
    ///
    /// # Errors
    ///
    /// As [`Paykit::denied`].
    pub(crate) async fn denied_for_user(&mut self, pubky: &[u8; 32]) -> crate::error::Result<bool> {
        let now = self.now();
        let read = self.local_get::<u64>(key::LISTS_READ).await?;
        let copy = self.local_get::<Vec<[u8; 32]>>(key::DENIED).await?;
        match (read, copy) {
            (Some(read), Some(denied)) if read <= now && now - read < LISTS_REFRESH_MS => {
                Ok(denied.contains(pubky))
            }
            _ => self.denied(pubky).await,
        }
    }

    /// Block `pubky`: the identity's deny list takes it (`deny.put`, the client's
    /// [`pubky_switch_client::app::App::deny`]), and the copy of the list this app judges
    /// payments by holds it at once, whatever its age (Paykit spec 8.1). A `deny.put` the
    /// client keeps unanswered ([`AppError::Kept`]) counts at once all the same: the copy
    /// holds it, and so does every fresh read of the list before an automatic payment until
    /// the list shows it (`bk/`). A block made through the client directly is seen at the
    /// next read of the lists.
    ///
    /// # Errors
    ///
    /// The client's errors but a kept batch, such as without `manage_contacts`: the copy is
    /// then as it was. A store failure after the switch took the block leaves it counted.
    pub async fn block(&mut self, pubky: &[u8; 32]) -> crate::error::Result<()> {
        self.refresh_own()?;
        self.list_own(pubky, true).await
    }

    /// Unblock `pubky`, as [`Paykit::block`] blocks: the deny list drops it, and the copy at
    /// once. Unblocking ends the pause (Paykit spec 8.1). A `deny.delete` the client keeps
    /// unanswered counts at once all the same, as a kept block does: the copy drops `pubky`,
    /// and so does every fresh read of the list before an automatic payment until the list
    /// shows it dropped (`bk/`). An unblock made through the client directly is seen at the
    /// next read of the lists.
    ///
    /// # Errors
    ///
    /// As [`Paykit::block`].
    pub async fn unblock(&mut self, pubky: &[u8; 32]) -> crate::error::Result<()> {
        self.refresh_own()?;
        self.list_own(pubky, false).await
    }

    /// Write `pubky` to the deny list (`blocked`) or drop it, and the copy with it; a batch
    /// the client keeps unanswered is marked (`bk/`) as [`Paykit::block`] says. The mark and
    /// the copy go in one commit, so no crash leaves the one without the other. Each fails
    /// closed: a block counts, marked and in the copy, before the switch is asked, and an
    /// unblock only once the switch took it or keeps it, so a commit that fails or a crash
    /// between the call and the commit leaves `pubky` counted as blocked.
    async fn list_own(&mut self, pubky: &[u8; 32], blocked: bool) -> crate::error::Result<()> {
        if blocked {
            return self.block_own(pubky).await;
        }
        let sent = self.app.undeny(pubky).await;
        let mut tx = Tx::default();
        match sent {
            Ok(()) => {
                tx.delete(&key::own_block(pubky));
            }
            Err(error) if error.kept_batch().is_some() => {
                tx.put(&key::own_block(pubky), &blocked)?;
            }
            Err(error) => return Err(error.into()),
        }
        self.copy_denied(&mut tx, pubky, blocked).await?;
        self.local_commit(tx).await
    }

    /// The block of [`Paykit::list_own`]: marked and in the copy first, then sent. The mark
    /// goes once the switch took it; a block the client neither applied nor kept puts the
    /// mark and the copy back as they were.
    async fn block_own(&mut self, pubky: &[u8; 32]) -> crate::error::Result<()> {
        let mark = key::own_block(pubky);
        let held = self.local_get::<bool>(&mark).await?;
        let copy = self.local_get::<Vec<[u8; 32]>>(key::DENIED).await?;
        let mut tx = Tx::default();
        tx.put(&mark, &true)?;
        self.copy_denied(&mut tx, pubky, true).await?;
        self.local_commit(tx).await?;
        let mut tx = Tx::default();
        match self.app.deny(pubky).await {
            Ok(()) => {
                tx.delete(&mark);
            }
            Err(error) if error.kept_batch().is_some() => return Ok(()),
            Err(error) => {
                match held {
                    Some(held) => tx.put(&mark, &held)?,
                    None => tx.delete(&mark),
                };
                match copy {
                    Some(copy) => tx.put(key::DENIED, &copy)?,
                    None => tx.delete(key::DENIED),
                };
                self.local_commit(tx).await?;
                return Err(error.into());
            }
        }
        self.local_commit(tx).await
    }

    /// Write again, once after a restore, each block and unblock this app made that the
    /// backup kept unanswered (`bk/`): the client's kept batch that would have sent it
    /// belongs to the device that kept it, and isn't restored, so without this the user's
    /// block would be lost at the next read of the lists (Paykit spec 8.1). Each counts at
    /// once meanwhile, as a kept one does. One the switch refuses, or that can't be sent
    /// now, keeps its mark, so it still counts, until the list shows it or the user changes
    /// it, and the pass runs again at the next receive: it is done only once each was
    /// written, or kept by the client for a later try. Only a store failure is an error.
    pub(crate) async fn send_own_blocks_again(&mut self) -> crate::error::Result<()> {
        let Some(mut hold) = self
            .local_get::<crate::hold::RestoreHold>(key::RESTORE_HOLD)
            .await?
        else {
            return Ok(());
        };
        if hold.own_blocks_sent {
            return Ok(());
        }
        let mut done = true;
        for (rest, blocked) in self.local_scan::<bool>(key::OWN_BLOCKS).await? {
            let pubky = pubky_switch_core::enc::unb64_array::<32>(&rest)
                .map_err(|_| crate::error::PaykitError::corrupt())?;
            match self.list_own(&pubky, blocked).await {
                // Neither written nor kept: tried again at the next receive.
                Err(crate::error::PaykitError::Client(error))
                    if !matches!(error.unkept(), AppError::Store(_) | AppError::Alarm(_)) =>
                {
                    done = false;
                }
                other => other?,
            }
        }
        if !done {
            return Ok(());
        }
        hold.own_blocks_sent = true;
        self.local_put(key::RESTORE_HOLD, &hold).await
    }

    /// The copy of the deny list with `pubky` held or not, put in `tx`.
    async fn copy_denied(
        &self,
        tx: &mut Tx,
        pubky: &[u8; 32],
        held: bool,
    ) -> crate::error::Result<()> {
        let mut denied = self
            .local_get::<Vec<[u8; 32]>>(key::DENIED)
            .await?
            .unwrap_or_default();
        denied.retain(|other| other != pubky);
        if held {
            denied.push(*pubky);
        }
        tx.put(key::DENIED, &denied)?;
        Ok(())
    }

    /// Whether every send to `pubky` is held back now (Paykit spec 8.1, "Blocked
    /// counterparties"): no list, renewal, answer or re-send request goes to it while the
    /// copy of the deny list holds it, nor while the copy isn't known, at an app holding
    /// `contacts`: no read of the lists succeeded since the last that failed, or since a
    /// restore (`lr` absent). An unknown copy decides nothing, so what is owed to that
    /// counterparty is kept, never dropped, and goes once a read shows it not blocked.
    pub(crate) async fn sends_held(&self, pubky: &[u8; 32]) -> crate::error::Result<bool> {
        Ok(!self.deny_copy_known().await? || self.denied_as_last_read(pubky).await?)
    }

    /// Whether the copy of the deny list is known ([`Paykit::sends_held`]): always at an app
    /// that doesn't read the lists, which blocks no one.
    pub(crate) async fn deny_copy_known(&self) -> crate::error::Result<bool> {
        if !reads_lists(self.app.cert().scopes()) {
            return Ok(true);
        }
        Ok(self.local_get::<u64>(key::LISTS_READ).await?.is_some())
    }

    /// Whether the copy of the deny list kept from the last read holds `pubky`, at an app
    /// holding `contacts`, for the views, which read nothing from the switch, and for the
    /// re-sends, which send a blocked counterparty nothing ([`Paykit::sends_held`]). The copy
    /// holds this app's own blocks and unblocks at once, kept unanswered included
    /// ([`Paykit::block`]), and a receive reads it again first once it is
    /// [`LISTS_REFRESH_MS`] old.
    pub(crate) async fn denied_as_last_read(&self, pubky: &[u8; 32]) -> crate::error::Result<bool> {
        if !reads_lists(self.app.cert().scopes()) {
            return Ok(false);
        }
        Ok(self
            .local_get::<Vec<[u8; 32]>>(key::DENIED)
            .await?
            .is_some_and(|denied| denied.contains(pubky)))
    }

    /// Place the list held for payee `payee`'s app `app_id` although its `payment_lists`
    /// record is deleted or doesn't read, accepting that the single-use marks it held are
    /// gone (Paykit wire 5.4): the list is written afresh, built on the version this app
    /// holds, with no endpoint marked used, so a single-use endpoint another own app already
    /// paid can be paid again. The host must show the user exactly that before calling it,
    /// with `confirmed` as the proof the user saw it. A record that changed since is read
    /// again and refused with [`crate::PaykitError::Conflict`]; one that reads by then has
    /// the list placed with its marks, as at a receive.
    ///
    /// # Errors
    ///
    /// [`crate::Refusal::NotFound`] when no list is held for that app;
    /// [`crate::Refusal::NotNow`] naming the scope keys while the record's value is sealed
    /// under a key the app doesn't hold yet: the list is placed with its marks once the
    /// value opens, at a receive; the client's errors.
    pub async fn place_held_list_without_marks(
        &mut self,
        payee: &[u8; 32],
        app_id: &[u8; 16],
        _confirmed: pubky_switch_client::app::UserConfirmed,
    ) -> crate::error::Result<()> {
        self.refresh_own()?;
        self.records_known().await?;
        let own = self.identity();
        let target = list_target(payee, app_id);
        let record_key = self.app.record_key(&target)?;
        let notice = key::held_list(&record_key);
        let list = self
            .local_get::<KeptMessage>(&notice)
            .await?
            .ok_or(crate::error::Refusal::NotFound)?
            .stored(&own)?;
        let held = self.app.shared_record(&record_key).await?;
        // A value only waiting for its scope key holds marks this app can't see yet: it is
        // never written over without them, and the list is placed with them once it opens.
        if held
            .as_ref()
            .is_some_and(|r| crate::hold::awaits_key(&r.content))
        {
            return Err(crate::hold::value_awaits_key());
        }
        let held = held
            .and_then(|r| r.content.value().map(<[u8]>::to_vec))
            .and_then(|value| PaymentListRecord::read(&value, &own).ok());
        let mut tx = Tx::default();
        tx.delete(&notice);
        let Some(record) = next_list_record(held, &list) else {
            return self.local_commit(tx).await;
        };
        let value = record.to_json()?;
        let op = self
            .app
            .record_op(&target, Some(&value), Vec::new())
            .await?;
        let batch = pubky_switch_client::app::OpBatch::new()
            .entry(op)
            .locals(tx.into_writes());
        if let Err(error) = self.app.submit_batch(batch).await {
            if error.code() == Some(pubky_switch_core::http::ErrorCode::Conflict) {
                self.app.read_record(&target).await?;
                return Err(crate::error::PaykitError::Conflict(
                    "the payment_lists record changed since it was seen".into(),
                ));
            }
            return Err(error.into());
        }
        Ok(())
    }

    /// Answer the `resend_request`s the receive step kept, oldest first, each once (Paykit
    /// spec 9). One from a requester the copy of the deny list holds is dropped unanswered,
    /// as v0.1 ran no send for a blocked peer (Paykit spec 8.1); while the copy isn't known
    /// ([`Paykit::deny_copy_known`]), none is answered or dropped: they wait for a read of
    /// the lists. Returns how many were answered.
    pub(crate) async fn answer_kept_resends(&mut self) -> crate::error::Result<u64> {
        let mut answered = 0u64;
        if !self.deny_copy_known().await? {
            return Ok(answered);
        }
        // Each read bound to its key, as a restore reads it: one that fails it is a corrupt
        // store, never an answer dropped.
        for (rest, queued) in self.local_scan::<QueuedAnswer>(key::ANSWERS).await? {
            let answer_key = format!("{}{rest}", key::ANSWERS);
            // A requester blocked since the request was kept is sent nothing (Paykit spec 8.1).
            let blocked = self.denied_as_last_read(&queued.requester).await?;
            if let (false, Ok(request)) = (blocked, ResendRequest::from_body(&queued.body)) {
                self.answer_resend(&queued.requester, &queued.cert, &request)
                    .await?;
                answered = answered.saturating_add(1);
            }
            self.local_delete(&answer_key).await?;
        }
        Ok(answered)
    }
}

/// The `unrequested` member of a payer's record of a payment without a request: its
/// destination, and the asset and amount being paid, which every app writes with the
/// `attempt` and the stored proof carries the same (Paykit wire 5.3).
pub(crate) fn unrequested_of(
    destination: &crate::journal::Destination,
    amount: &paykit_lib::PaymentAmount,
) -> crate::error::Result<UnrequestedDestination> {
    Ok(UnrequestedDestination {
        app_id: destination.app_id,
        endpoint_id: destination.endpoint_id()?,
        payload: destination.payload()?,
        source: destination.source()?,
        list_version: destination.list_version,
        single_use: destination.single_use,
        amount: Some(amount.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` replies from `sender`, accepted from `from` on.
    fn replies(sender: u8, n: usize, from: u64) -> Vec<CountedReply> {
        (0..n)
            .map(|i| ([sender; 32], from + u64::try_from(i).unwrap()))
            .collect()
    }

    #[test]
    fn test_kept_reply_evicted_none_within_both_bounds() {
        let kept = replies(1, MAX_KEPT_REPLIES_PER_SENDER - 1, 10);
        assert_eq!(kept_reply_evicted(&kept, &[1; 32]), None);
        let mut kept = replies(1, MAX_KEPT_REPLIES_PER_SENDER, 10);
        kept.extend(replies(
            2,
            MAX_KEPT_REPLIES - MAX_KEPT_REPLIES_PER_SENDER - 1,
            5,
        ));
        assert_eq!(kept_reply_evicted(&kept, &[3; 32]), None);
    }

    #[test]
    fn test_kept_reply_evicted_past_a_senders_bound_its_earliest() {
        let mut kept = replies(2, 3, 1);
        kept.extend(replies(1, MAX_KEPT_REPLIES_PER_SENDER, 10));
        assert_eq!(kept_reply_evicted(&kept, &[1; 32]), Some(3));
    }

    #[test]
    fn test_kept_reply_evicted_past_the_total_the_earliest_of_the_largest() {
        // Bob holds 64, alice 63 and carol 1: a reply from carol drops bob's earliest.
        let mut kept = replies(1, MAX_KEPT_REPLIES_PER_SENDER - 1, 100);
        kept.extend(replies(2, MAX_KEPT_REPLIES_PER_SENDER, 500));
        kept.push(([3; 32], 7));
        assert_eq!(
            kept_reply_evicted(&kept, &[3; 32]),
            Some(MAX_KEPT_REPLIES_PER_SENDER - 1)
        );
        // Tied, the lower sender identity gives, wherever its replies come.
        let mut tied = replies(2, MAX_KEPT_REPLIES_PER_SENDER, 100);
        tied.extend(replies(1, MAX_KEPT_REPLIES_PER_SENDER, 500));
        assert_eq!(
            kept_reply_evicted(&tied, &[3; 32]),
            Some(MAX_KEPT_REPLIES_PER_SENDER)
        );
        let mut tied = replies(1, MAX_KEPT_REPLIES_PER_SENDER, 500);
        tied.extend(replies(2, MAX_KEPT_REPLIES_PER_SENDER, 100));
        assert_eq!(kept_reply_evicted(&tied, &[3; 32]), Some(0));
    }
}
