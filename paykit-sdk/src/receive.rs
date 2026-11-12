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
use paykit_lib::records::{PaymentListRecord, StoredMessage, UnrequestedDestination};
use paykit_lib::{AttemptId, Body};
use pubky_switch_client::app::{
    AppError, ClaimFetch, ClaimOp, LocalWrite, Need, ObligationDeadline, ReceiveContext,
    RecordContent, RecordTarget, Refusal as ClientRefusal, StoreFailure, Unopened,
};
use pubky_switch_client::error::StoreError;
use pubky_switch_core::claims::{Basis, ClaimStatus, Outcome, RECONCILIATION_MS};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::records::AppCert;
use pubky_switch_core::scopes::Permission;
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::activity::{self, ActivityTarget, Role};
use crate::hold::RecordSide;
use crate::journal::JournalEntry;
use crate::marks::{count_dropped, read_mark, MarkFamily};
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
    /// the app doesn't hold yet ([`wait_for_key`]): the entry waits for the key, and is given
    /// up once [`crate::KEPT_MESSAGE_MS`] have passed since its acceptance
    /// (`PaykitProtocol`'s receive step).
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
/// The settlement ID of a proof's `proof` object, by the app's rail ([`crate::Rail::settlement_id`]).
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
            if payee && payer_reply(cx, stored, &scopes, settlement)? {
                // A payer's cancellation of a request this app sent.
            } else if pay {
                // One for a request the app holds no record of is new state, which the spam
                // rule gates. While the copy of the payer's records may lack records, the
                // absence decides nothing: the entry is held.
                if cx.record(&payer_target.record_target())?.is_none() {
                    hold_while_unlisted(cx, RecordSide::Payer)?;
                }
                if admits_new_state(cx, &sender, &scopes)? {
                    keep_cancellation(cx, &payer_target, stored, false)?;
                }
            }
        }
        Body::PaymentReceipt(receipt) if pay => {
            receipt_effect(cx, stored, receipt.attempt_id)?;
        }
        Body::PrivatePaymentList(_) if pay => {
            let target = list_target(&sender, &stored.message().from.app_id);
            let record = cx.record(&target)?;
            if record.is_none() {
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
        // Allowances (Paykit spec 8.3): the four messages go into the `allowances` record, an
        // end kept first as the stop; a notice changes only the executor's consent list.
        Body::AllowanceProposal(_)
        | Body::AllowanceAcceptance(_)
        | Body::AllowanceRejection(_)
        | Body::AllowanceEnd(_)
            if scopes.iter().any(|s| s == "allowance") =>
        {
            crate::allowances::receive_effect(cx, stored, &scopes)?;
        }
        Body::ExecutorNotice(notice) if scopes.iter().any(|s| s == "allowance_execute") => {
            crate::allowances::notice_effect(cx, notice)?;
        }
        // A publishing app answers a request naming a used version with its current list
        // (Paykit spec 8.4, "Publishing"). This SDK publishes no list at all yet, so the
        // request is taken with no effect; the answer comes with list publishing.
        Body::PrivatePaymentListRequest(_) => {}
        _ => {}
    }
    Ok(())
}

pub(crate) fn local<T: Serialize>(key: &str, value: &T) -> Result<LocalWrite, Fail> {
    state::local_put(key, value).map_err(|_| Fail::corrupt())
}

/// Add `write` to the step's local writes. Past the step's bound
/// ([`pubky_switch_client::app::MAX_STEP_LOCAL_BYTES`]) the context refuses it and marks
/// the step; the step goes on, and the client stores the entry as too large once the step
/// applies (Switch spec 8.2). Dropping the message here instead would consume a valid
/// entry, so the refusal is not passed on.
pub(crate) fn keep_local(cx: &mut ReceiveContext<'_>, write: LocalWrite) {
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
    let held = match cx.local_get(key::CURSOR_TIME.as_bytes())? {
        Some(bytes) => state::decode::<u64>(&bytes).map_err(|_| Fail::corrupt())?,
        None => 0,
    };
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
/// clean sender.
pub(crate) fn admits_new_state(
    cx: &mut ReceiveContext<'_>,
    sender: &[u8; 32],
    scopes: &[String],
) -> Result<bool, Fail> {
    if !reads_lists(scopes) {
        return Ok(true);
    }
    let unknown = || Fail::Store(StoreFailure::unavailable(StoreError::Unavailable));
    if cx.local_get(key::LISTS_READ.as_bytes())?.is_none() {
        return Err(unknown());
    }
    let standing = cx.sender_disposition(sender)?;
    if standing.lists_as_of_ms.is_none() {
        return Err(unknown());
    }
    if !(standing.blocked || standing.flagged) {
        return Ok(true);
    }
    let kept = key::spam(sender);
    let count = match cx.local_get(kept.as_bytes())? {
        Some(bytes) => state::decode::<u64>(&bytes).map_err(|_| Fail::corrupt())?,
        None => 0,
    };
    keep_local(cx, local(&kept, &count.saturating_add(1))?);
    Ok(false)
}

/// Whether an app with `scopes` reads the lists: `contacts` gives `lists.read`.
pub(crate) fn reads_lists(scopes: &[String]) -> bool {
    scopes.iter().any(|s| s == "contacts")
}

/// Note a counterparty with Paykit activity, for re-sends after a `missed` item or a
/// provider loss (Paykit spec 9, Switch spec 12.6).
pub(crate) fn note_peer(
    cx: &mut ReceiveContext<'_>,
    pubky: &[u8; 32],
    stored: &StoredMessage,
) -> Result<(), Fail> {
    keep_local(
        cx,
        local(&key::peer(pubky), &stored.accepted_at().unwrap_or(0))?,
    );
    Ok(())
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
    wait_for_record_key(cx, &target.record_target())
}

/// [`wait_for_key`] for the record at any client target, such as an `allowances` record.
pub(crate) fn wait_for_record_key(
    cx: &ReceiveContext<'_>,
    target: &RecordTarget,
) -> Result<(), Fail> {
    let held = cx.record(target)?;
    if let Some(RecordContent::Unreadable(Unopened::KeyNotHeld { data_epoch })) =
        held.map(|held| held.content)
    {
        return Err(Fail::AwaitsKey { data_epoch });
    }
    Ok(())
}

/// Whether a message accepted at `accepted_at` that waits for a key is given up now: once
/// [`KEPT_MESSAGE_MS`] have passed since its acceptance, on the highest service time
/// verified (Paykit spec 8.1).
pub(crate) fn key_wait_over(cx: &ReceiveContext<'_>, accepted_at: Option<u64>) -> bool {
    accepted_at.is_none_or(|at| {
        cx.highest_service_time()
            .is_some_and(|t| at.saturating_add(KEPT_MESSAGE_MS) <= t)
    })
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
/// An Allowance end kept as the stop follows the same rule (`allowances.rs`).
pub(crate) fn holds_record(content: Option<&RecordContent>) -> bool {
    content.is_some_and(|content| *content != RecordContent::Deleted)
}

/// The value held at `target`, which is of `side`: `None` for none, or one that isn't
/// open or is deleted. The entry is held instead while the value waits for its scope key
/// ([`wait_for_key`]), and while the record is absent and the app's copy of `side` may
/// lack records ([`hold_while_unlisted`]): the absence decides nothing then.
fn held_or_unlisted(
    cx: &ReceiveContext<'_>,
    target: &ActivityTarget,
    side: RecordSide,
) -> Result<Option<Vec<u8>>, Fail> {
    held_or_unlisted_at(cx, &target.record_target(), side)
}

/// [`held_or_unlisted`] for the record at any client target, such as an `allowances`
/// record, of `side`.
pub(crate) fn held_or_unlisted_at(
    cx: &ReceiveContext<'_>,
    target: &RecordTarget,
    side: RecordSide,
) -> Result<Option<Vec<u8>>, Fail> {
    wait_for_record_key(cx, target)?;
    let record = cx.record(target)?;
    if record.is_none() {
        hold_while_unlisted(cx, side)?;
    }
    Ok(record.and_then(|held| held.content.value().map(<[u8]>::to_vec)))
}

/// Hold the entry while the app's copy of `side`'s records may lack records (Paykit spec
/// 8.1, "Records not listed yet"): after a restore, a `missed` item, or a certificate that
/// reads more, until Paykit has listed them. The rule is [`crate::hold::side_unlisted`], read
/// per entry from the context's certificate and Paykit's state (`nl`, `ml`, `lc`), so an
/// entry after a wider certificate or a `missed` item the same read took is held too. The entry is held as for a store
/// unavailable for now (Paykit spec 17, question 27), and read again at a later read.
///
/// [`Paykit::side_unlisted`] counts the records as listed under a certificate Paykit
/// refuses with none kept as listed under; this needs no such case: [`effect`] holds every
/// entry under a certificate that fails the same check before any branch reaches here, so
/// the two agree wherever this runs.
pub(crate) fn hold_while_unlisted(cx: &ReceiveContext<'_>, side: RecordSide) -> Result<(), Fail> {
    let marked = cx.local_get(key::RECORDS_UNLISTED.as_bytes())?.is_some()
        || cx.local_get(key::MISSED_UNLISTED.as_bytes())?.is_some();
    let listed = match cx.local_get(key::LISTED_CERT.as_bytes())? {
        None => None,
        Some(bytes) => {
            let signed: SignedDoc = state::decode(&bytes).map_err(|_| Fail::corrupt())?;
            Some(AppCert::verify(&signed, &cx.identity()).map_err(|_| Fail::corrupt())?)
        }
    };
    if crate::hold::side_unlisted(side, marked, listed.as_ref(), cx.cert()) {
        return Err(Fail::Store(StoreFailure::unavailable(
            StoreError::Unavailable,
        )));
    }
    Ok(())
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
/// those and together with the Allowance ends kept as stops for Allowances not held
/// ([`make_room_for_stop`]): past its sender's [`MAX_KEPT_REPLIES_PER_SENDER`] such stops,
/// that sender's earliest goes, and past [`MAX_KEPT_REPLIES`] in all, the earliest of the
/// sender keeping the most ([`kept_reply_evicted`]). It goes [`KEPT_MESSAGE_MS`] after the cancellation's acceptance
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
    if cx.local_get(kept.as_bytes())?.is_some() {
        return Ok(());
    }
    if !held {
        make_room_for_stop(cx, &stored.sender())?;
    }
    keep_local(cx, local(&kept, &KeptMessage::of(stored))?);
    Ok(())
}

/// Make room for a new stop from `sender` among the stops kept for records not held
/// ([`holds_record`]), in the step: the one the bounds drop ([`kept_reply_evicted`]) goes.
/// The payees' cancellations of requests not held (`kc/`) and the counterparties' ends of
/// Allowances not held (`ae/`, `allowances.rs`) are counted together, under one sender's
/// bound and the bound in all. An Allowance's stop whose Allowance has a stored message
/// kept to write ([`crate::allowances::keeps_stored_message`]) is neither counted nor
/// dropped: the message could open the record, and the stop must then be there.
pub(crate) fn make_room_for_stop(
    cx: &mut ReceiveContext<'_>,
    sender: &[u8; 32],
) -> Result<(), Fail> {
    let mut kept = Vec::new();
    let mut names = Vec::new();
    for prefix in [key::KEPT_CANCELLATIONS, key::ALLOWANCE_ENDS] {
        for (bytes, value) in cx.local_scan(prefix.as_bytes())? {
            let (record, allowance) = match crate::state::StateKey::parse(&bytes) {
                Some(crate::state::StateKey::KeptCancellation(record)) => (record, false),
                Some(crate::state::StateKey::AllowanceEnd(record)) => (record, true),
                _ => return Err(Fail::corrupt()),
            };
            let stop = state::decode::<KeptMessage>(&value).map_err(|_| Fail::corrupt())?;
            if holds_record(cx.record_at(&record)?.map(|held| held.content).as_ref()) {
                continue;
            }
            if allowance && crate::allowances::keeps_stored_message(cx, &record)? {
                continue;
            }
            kept.push((stop.sender(), stop.accepted_at().unwrap_or(0)));
            names.push(bytes);
        }
    }
    if let Some(index) = kept_reply_evicted(&kept, sender) {
        keep_local(
            cx,
            LocalWrite::Delete {
                key: names.swap_remove(index),
            },
        );
    }
    Ok(())
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
                    let record = cx.record_key(&target.record_target())?;
                    let kept = key::kept_reply(&sender, &record, &stored.message().msg_id);
                    if cx.local_get(kept.as_bytes())?.is_none() {
                        make_room_for_reply(cx, &sender)?;
                        keep_local(cx, local(&kept, &KeptMessage::of(stored))?);
                    }
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

/// A receipt (Paykit spec 7.3, 8.4): stored in the payer's record and, in the same batch,
/// the attempt it names settled by evidence when it is open, unknown or failed and the
/// claim isn't SETTLED (Switch spec 10.2 row 3). Once the claim is gone, a receipt for a
/// payment made after `failed` was written raises the notice "marked failed, was paid"
/// instead (Paykit spec 7.2).
fn receipt_effect(
    cx: &mut ReceiveContext<'_>,
    stored: &StoredMessage,
    attempt_id: AttemptId,
) -> Result<(), Fail> {
    let sender = stored.sender();
    let own = cx.identity();
    let target = match stored.message().refs.get(1) {
        Some(request) => ActivityTarget::payer(natural_key::request_record(&sender, request)),
        None => ActivityTarget::payer(natural_key::payer_payment_record(&own, &attempt_id)),
    };
    // The payer matches a receipt through its record, whose counterparty must be the
    // receipt's sender (Paykit spec 8.4).
    let Some(value) = held_or_unlisted(cx, &target, RecordSide::Payer)? else {
        return Ok(());
    };
    let Some(record) = activity::read(Some(&value), &own, target.side.side()) else {
        return Ok(());
    };
    if record.counterparty != sender {
        return Ok(());
    }
    // A receipt names the proof it confirms first (Paykit wire 4.11), and the record holds a
    // receipt only beside that proof (Paykit wire 5.3): the payer's proof goes into the
    // record in the batch that sends it, so a receipt for a record without it, or naming
    // another proof, isn't stored. Stored, it would leave the payer unable to record its own
    // proof. It still settles the attempt it names.
    let names_held_proof = record
        .proof
        .as_ref()
        .is_some_and(|proof| stored.message().refs.first() == Some(&proof.hash()));
    // The record shows the payment made after `failed` was written on its claim, and no
    // receipt yet (placing one sets the status `receipted`): what every app that gets the
    // receipt tells the user once the claim is gone (Paykit spec 7.2, 8.3).
    let first_after_failed =
        names_held_proof && record.status.as_deref() == Some(PAID_AFTER_FAILED);
    if names_held_proof {
        place(cx, &target, sender, Role::Receipt, stored)?;
    }
    // The journal entry of the attempt, if this app made it: it holds a payment without a
    // request's deadline, and whether `failed` stands on the claim of a payment made. One
    // that doesn't read parks the receipt rather than settling the claim with no deadline,
    // or consuming it (Switch spec 8.2): the store is corrupt.
    let journaled = match cx.local_get(key::journal(&attempt_id).as_bytes())? {
        Some(bytes) => Some(
            state::decode::<JournalEntry>(&bytes)
                .and_then(|entry| entry.check().map(|()| entry))
                .map_err(|_| Fail::corrupt())?,
        ),
        None => None,
    };
    // A receipt reaches only an app holding `pay`, which carries `claims.write`, and an entry
    // that comes under a certificate without it is held unprocessed: this returns only for a
    // certificate Paykit's checks would refuse.
    if !cx.cert().has_permission(Permission::ClaimsWrite) {
        return Ok(());
    }
    let (obligation, deadline) = match stored.message().refs.get(1) {
        Some(request) => {
            let deadline = crate::requests::request_of(&record)
                .and_then(|(_, body)| body.attempt_deadline(None).ok())
                .map_or(ObligationDeadline::NotKnown, crate::pay::own_deadline);
            (obligation(cx, &sender, request, 0), deadline)
        }
        None => {
            // The deadline of a payment without a request is the one its journal kept.
            let deadline = journaled
                .as_ref()
                .map_or(ObligationDeadline::NotKnown, |e| {
                    crate::pay::own_deadline(e.deadline)
                });
            (
                paykit_lib::derive::payment_obligation(&attempt_id),
                deadline,
            )
        }
    };
    // A claim the app keeps no view of is read from the switch first, so the receipt
    // settles it in the cursor's batch (Switch spec 10.2 row 3).
    let view = match cx.claim_view(&obligation)? {
        Some(view) => Some(view),
        None => match cx.switch_claim(&obligation) {
            None => return Err(Fail::Need(vec![Need::Claim(obligation)])),
            Some(ClaimFetch::Read(read)) => Some(read.view.clone()),
            Some(ClaimFetch::Absent) => None,
            // Refused, such as without `claims.read`: nothing to settle here.
            Some(_) => return Ok(()),
        },
    };
    // The claim is gone, or may be: the switch holds none, or its pinned deadline is 30 days
    // or more before the highest service time verified, when the switch may delete it
    // (Switch spec 6.3). The receipt can't be recorded on it. For a payment made after
    // `failed` was written on it, the user is told it was paid (Paykit spec 7.2, "What a
    // payment method declares"); the receipt is in the record already.
    let gone = view.as_ref().is_none_or(|view| {
        view.deadline.is_some_and(|deadline| {
            cx.highest_service_time()
                .is_some_and(|t| deadline.saturating_add(RECONCILIATION_MS) <= t)
        })
    });
    if gone {
        let waiting = journaled.filter(|entry| entry.failed_on_claim);
        if waiting.is_some() || first_after_failed {
            let late = KeptNotice::late(attempt_id, sender, stored.message().refs.get(1), 0);
            late_receipt(cx, late, waiting)?;
        }
        return Ok(());
    }
    let Some(view) = view else {
        return Ok(());
    };
    // A claim SETTLED, or settled by this receipt, holds the payment: an entry waiting for
    // its receipt no longer waits, in the same batch.
    let waiting = journaled.filter(|entry| entry.failed_on_claim);
    if view.status == ClaimStatus::Settled {
        return waiting.map_or(Ok(()), |entry| received_in_time(cx, entry));
    }
    let open = view.attempts.iter().any(|a| {
        a.attempt_id == attempt_id.to_bytes()
            && matches!(
                a.outcome,
                Outcome::Open | Outcome::Unknown | Outcome::Failed
            )
    });
    if open {
        let op = ClaimOp::done(
            obligation,
            view.step,
            attempt_id.to_bytes(),
            Basis::Evidence,
            Some(stored.hash()),
        )?;
        cx.claim(op, Vec::new(), deadline)?;
        if let Some(entry) = waiting {
            received_in_time(cx, entry)?;
        }
    }
    Ok(())
}

/// The notice "marked failed, was paid", `late`, raised with the cursor move once (Paykit
/// spec 7.2), and the payment's journal entry, if this app holds it `waiting`, kept as no
/// longer waiting for a receipt.
fn late_receipt(
    cx: &mut ReceiveContext<'_>,
    mut late: KeptNotice,
    waiting: Option<JournalEntry>,
) -> Result<(), Fail> {
    let attempt =
        AttemptId::parse(late.attempt_id.as_deref().unwrap_or("")).map_err(|_| Fail::corrupt())?;
    let notice = key::notice(&attempt, NoticeKind::PaidAfterFailed);
    if cx.local_get(notice.as_bytes())?.is_none() {
        late.raised_at_ms = cx.highest_service_time().unwrap_or(0);
        keep_local(cx, local(&notice, &late)?);
    }
    match waiting {
        Some(entry) => received_in_time(cx, entry),
        None => Ok(()),
    }
}

/// The `status` of a payer's record whose payment was made after `failed` was written on its
/// claim, which stays `failed` until a receipt settles it (Paykit wire 5.3: informational).
pub(crate) const PAID_AFTER_FAILED: &str = "failed";

/// The payment `entry` journals no longer waits for a receipt, kept with the cursor move.
fn received_in_time(cx: &mut ReceiveContext<'_>, mut entry: JournalEntry) -> Result<(), Fail> {
    let attempt = entry.attempt().map_err(|_| Fail::corrupt())?;
    entry.failed_on_claim = false;
    keep_local(cx, local(&key::journal(&attempt), &entry)?);
    Ok(())
}

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
    if let Some(bytes) = cx.local_get(kept.as_bytes())? {
        let held = state::decode::<KeptMessage>(&bytes)
            .and_then(|k| k.stored(&cx.identity()))
            .map_err(|_| Fail::corrupt())?;
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
/// requests the app holds no record of, and the counterparties' Allowance ends kept as stops
/// for Allowances it holds no record of, are bounded the same way, counted together and
/// apart from the replies.
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
/// bounds drop ([`kept_reply_evicted`]) goes, dropped without being processed, and counts in
/// its sender's mark (`pd/<sender>`, `marks.rs`) with its acceptance time, so the sender is
/// asked to send it again once the record of one of its requests is held.
pub(crate) fn make_room_for_reply(
    cx: &mut ReceiveContext<'_>,
    sender: &[u8; 32],
) -> Result<(), Fail> {
    let mut kept = Vec::new();
    let mut names = Vec::new();
    for (bytes, value) in cx.local_scan(key::KEPT_REPLIES.as_bytes())? {
        let Some(crate::state::StateKey::KeptReply { sender, .. }) =
            crate::state::StateKey::parse(&bytes)
        else {
            return Err(Fail::corrupt());
        };
        let reply = state::decode::<KeptMessage>(&value).map_err(|_| Fail::corrupt())?;
        kept.push((sender, reply.accepted_at().unwrap_or(0)));
        names.push(bytes);
    }
    let Some(index) = kept_reply_evicted(&kept, sender) else {
        return Ok(());
    };
    let (from, at) = kept[index];
    keep_local(
        cx,
        LocalWrite::Delete {
            key: names.swap_remove(index),
        },
    );
    let family = MarkFamily::Replies;
    let mark_key = family.key(&from);
    let held = match cx.local_get(mark_key.as_bytes())? {
        Some(value) => Some(read_mark(family, &mark_key, &value).map_err(|_| Fail::corrupt())?),
        None => None,
    };
    let mut others = Vec::new();
    if held.is_none() {
        for (bytes, value) in cx.local_scan(family.prefix().as_bytes())? {
            let name = String::from_utf8(bytes).map_err(|_| Fail::corrupt())?;
            let mark = read_mark(family, &name, &value).map_err(|_| Fail::corrupt())?;
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
/// acceptance, the time its sender keeps it to send again (Paykit spec 9).
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
    /// A record that still doesn't read, or is still deleted, keeps its list held; a list no
    /// newer than the record's is dropped. A placement that fails now is tried again at the
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
    /// batch (Switch spec 8.2, Paykit spec 8.1). One whose record doesn't come within
    /// [`KEPT_MESSAGE_MS`] of its acceptance, on the highest service time verified, is
    /// dropped: the payer keeps what it sent that long, and may send it again (Paykit spec
    /// 9). A placement that fails now is tried again at the next receive.
    pub(crate) async fn place_kept_replies(&mut self) -> crate::error::Result<()> {
        let own = self.identity();
        for (rest, kept) in self.local_scan::<KeptMessage>(key::KEPT_REPLIES).await? {
            let kept_key = format!("{}{rest}", key::KEPT_REPLIES);
            let reply = kept.stored(&own)?;
            // An Allowance's answer kept before its record (`allowances.rs`).
            if crate::allowances::is_allowance_response(reply.body()) {
                self.place_kept_allowance_response(&kept_key, reply).await?;
                continue;
            }
            let (Some(request), Some(role)) = (
                reply.message().refs.first(),
                crate::activity::Role::of(reply.body()),
            ) else {
                self.local_delete(&kept_key).await?;
                continue;
            };
            let target = ActivityTarget::payee(
                natural_key::request_record(&own, request),
                reply.body().label(),
            );
            if self.held_activity(&target).await?.is_none() {
                let stale = reply.accepted_at().is_none_or(|at| {
                    self.service_time()
                        .is_some_and(|t| at.saturating_add(KEPT_MESSAGE_MS) <= t)
                });
                if stale {
                    self.local_delete(&kept_key).await?;
                }
                continue;
            }
            let mut tx = Tx::default();
            tx.delete(&kept_key);
            let sender = reply.sender();
            match self
                .placed_write(&target, sender, role, reply, Vec::new())
                .await?
            {
                Some(entry) => {
                    let batch = pubky_switch_client::app::OpBatch::new()
                        .entry(entry)
                        .locals(tx.into_writes());
                    let _ = self.app.submit_batch(batch).await;
                }
                None => self.local_commit(tx).await?,
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
                let mut tx = Tx::default();
                tx.put(key::LISTS_READ, &now)?
                    .put(key::DENIED, &lists.deny)?;
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
        let mut tx = Tx::default();
        tx.put(key::LISTS_READ, &self.now())?
            .put(key::DENIED, &lists.deny)?;
        self.local_commit(tx).await?;
        Ok(lists.deny.contains(pubky))
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
    /// payments by holds it at once, whatever its age (Paykit spec 8.1). A block made
    /// through the client directly is seen at the next read of the lists.
    ///
    /// # Errors
    ///
    /// The client's errors, such as without `manage_contacts`; nothing is written then.
    pub async fn block(&mut self, pubky: &[u8; 32]) -> crate::error::Result<()> {
        self.refresh_own()?;
        self.app.deny(pubky).await?;
        self.copy_denied(pubky, true).await
    }

    /// Unblock `pubky`, as [`Paykit::block`] blocks: the deny list drops it, and the copy at
    /// once. Unblocking ends the pause (Paykit spec 8.1). An unblock made through the client
    /// directly is seen at the next read of the lists.
    ///
    /// # Errors
    ///
    /// As [`Paykit::block`].
    pub async fn unblock(&mut self, pubky: &[u8; 32]) -> crate::error::Result<()> {
        self.refresh_own()?;
        self.app.undeny(pubky).await?;
        self.copy_denied(pubky, false).await
    }

    /// The copy of the deny list with `pubky` held or not.
    async fn copy_denied(&mut self, pubky: &[u8; 32], held: bool) -> crate::error::Result<()> {
        let mut denied = self
            .local_get::<Vec<[u8; 32]>>(key::DENIED)
            .await?
            .unwrap_or_default();
        denied.retain(|other| other != pubky);
        if held {
            denied.push(*pubky);
        }
        self.local_put(key::DENIED, &denied).await
    }

    /// Whether the copy of the deny list kept from the last read holds `pubky`, at an app
    /// holding `contacts`, for the views, which read nothing from the switch.
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
    /// spec 9). Returns how many were answered.
    pub(crate) async fn answer_kept_resends(&mut self) -> crate::error::Result<u64> {
        let mut answered = 0u64;
        for (rest, queued) in self.local_scan::<QueuedAnswer>(key::ANSWERS).await? {
            if let Ok(request) = ResendRequest::from_body(&queued.body) {
                self.answer_resend(&queued.requester, &queued.cert, &request)
                    .await?;
                answered = answered.saturating_add(1);
            }
            let mut tx = Tx::default();
            tx.delete(&format!("{}{rest}", key::ANSWERS));
            self.local_commit(tx).await?;
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
