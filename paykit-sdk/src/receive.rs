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
//!   receipt and a Private Payment List go into theirs. A cancellation of a request the app
//!   holds no record of yet is kept, and placed in the batch that opens the record. A
//!   receipt also settles the attempt
//!   it names, by evidence (Switch spec 10.2 row 3), in the same batch: only the payee's yes
//!   counts (Paykit spec 7.3).
//! - **payee side** (`request` or a label): the payer's acceptance, rejection, cancellation
//!   and proof go into the request record under the request's label; a proof of a payment
//!   without a request into the record of its settlement, so one settlement is credited
//!   once (Paykit spec 8.4).
//! - a `resend_request` is kept, with the cursor move, for [`crate::Paykit::receive`] to
//!   answer (Paykit spec 9): answering sends messages, which no batch carries.
//!
//! Every write builds on the record the app holds. A record that already reflects a message
//! was written by another own app: nothing more is written, and only the cursor moves. When
//! another own app wrote the record meanwhile, the batch fails `409 conflict`, the client
//! reads the record again and calls the protocol once more, and the effect is built again
//! on what the switch holds.

use paykit_lib::bodies::ResendRequest;
use paykit_lib::derive::{natural_key, request_obligation_blinded};
use paykit_lib::records::{ActivitySide, PaymentListRecord, StoredMessage, UnrequestedDestination};
use paykit_lib::{AttemptId, Body};
use pubky_switch_client::app::{
    AppError, ClaimFetch, ClaimOp, LocalWrite, Need, ObligationDeadline, ReceiveContext,
    Refusal as ClientRefusal,
};
use pubky_switch_core::claims::{Basis, ClaimStatus, ClaimView, Outcome};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::scopes::Permission;
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::activity::{self, ActivityTarget, Role};
use crate::journal::JournalEntry;
use crate::paykit::Paykit;
use crate::protocol::writes_payee_label;
use crate::records::{list_target, KeptMessage};
use crate::state::{self, key, Tx};

/// Why an effect wasn't built.
pub(crate) enum Fail {
    /// A write can't be built now, for want of a scope key: the client keeps the entry and
    /// hands it over again (Switch spec 3.5).
    NotNow,
    /// The effect can't be built: the entry is dropped, with the rule.
    Drop(String),
    /// The effect needs these claims read from the switch first (Switch spec 10.2): the
    /// client reads them and calls the step again.
    Need(Vec<Need>),
}

impl From<AppError> for Fail {
    fn from(error: AppError) -> Self {
        match error {
            AppError::Refused(ClientRefusal::NotNow(_)) => Self::NotNow,
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
pub(crate) fn effect(
    cx: &mut ReceiveContext<'_>,
    stored: &StoredMessage,
    seq: u64,
) -> Result<(), Fail> {
    let sender = stored.sender();
    let scopes = cx.cert().scopes().to_vec();
    let pay = scopes.iter().any(|s| s == "pay");
    let payee = paykit_lib::scopes::acts_as_payee(&scopes);
    match stored.body() {
        Body::PaymentRequest(_) if pay => {
            let target =
                ActivityTarget::payer(natural_key::request_record(&sender, &stored.hash()));
            place_request(cx, &target, sender, stored)?;
            note_peer(cx, &sender, stored)?;
        }
        Body::PaymentRequestAcceptance(_)
        | Body::PaymentRequestRejection(_)
        | Body::PaymentProof(_)
            if payee =>
        {
            payer_reply(cx, stored, &scopes)?;
        }
        Body::PaymentRequestCancellation(_) => {
            let Some(request) = stored.message().refs.first().copied() else {
                return Ok(());
            };
            let payer_target =
                ActivityTarget::payer(natural_key::request_record(&sender, &request));
            if pay && held_value(cx, &payer_target)?.is_some() {
                place(cx, &payer_target, sender, Role::Cancellation, stored)?;
            } else if payee && payer_reply(cx, stored, &scopes)? {
                // A payer's cancellation of a request this app sent.
            } else if pay {
                keep_cancellation(cx, &payer_target, stored)?;
            }
        }
        Body::PaymentReceipt(receipt) if pay => {
            receipt_effect(cx, stored, receipt.attempt_id)?;
        }
        Body::PrivatePaymentList(_) if pay => {
            list_effect(cx, stored)?;
            note_peer(cx, &sender, stored)?;
        }
        Body::ResendRequest(_) => {
            let queued = QueuedAnswer {
                requester: sender,
                cert: stored.cert().signed().clone(),
                body: stored.message().body.clone(),
            };
            cx.local(local(&key::answer(seq, &stored.message().msg_id), &queued)?);
        }
        _ => {}
    }
    Ok(())
}

fn local<T: Serialize>(key: &str, value: &T) -> Result<LocalWrite, Fail> {
    state::local_put(key, value).map_err(|_| Fail::Drop("the effect can't be kept".into()))
}

/// Note a counterparty with Paykit activity, for re-sends after a `missed` item or a
/// provider loss (Paykit spec 9, Switch spec 12.6).
fn note_peer(
    cx: &mut ReceiveContext<'_>,
    pubky: &[u8; 32],
    stored: &StoredMessage,
) -> Result<(), Fail> {
    cx.local(local(
        &key::peer(pubky),
        &stored.accepted_at().unwrap_or(0),
    )?);
    Ok(())
}

/// The value of the record the app holds at `target`, if it holds a readable one.
fn held_value(cx: &ReceiveContext<'_>, target: &ActivityTarget) -> Result<Option<Vec<u8>>, Fail> {
    Ok(cx
        .record(&target.record_target())?
        .and_then(|held| held.content.value().map(<[u8]>::to_vec)))
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

/// A payee's cancellation of a request the payer holds no record of yet, kept with the
/// cursor move until the request comes (Paykit spec 8.1): a request first unopenable and
/// re-sent later (Paykit spec 9) then opens its record already cancelled, in the batch that
/// writes it ([`place_request`]), and is never payable. A later cancellation of the same
/// request adds nothing; one record holds one per role (Paykit spec 6).
///
/// Security: the cancellation passed every recipient check and check 10 before it was kept,
/// and is verified again when it is placed. It is keyed by the payer's record of the request
/// it names first in `refs`, which derives from the sender and that hash, so it can only
/// cancel that sender's request. It is kept until its request comes: a request record can
/// still come from another own app after the sender stops re-sending, and a cancelled
/// request must never become payable. Admission bounds what a sender can make the app keep
/// (Switch spec 7).
fn keep_cancellation(
    cx: &mut ReceiveContext<'_>,
    target: &ActivityTarget,
    stored: &StoredMessage,
) -> Result<(), Fail> {
    let kept = key::kept_cancellation(&cx.record_key(&target.record_target())?);
    if cx.local_get(kept.as_bytes())?.is_some() {
        return Ok(());
    }
    cx.local(local(&kept, &KeptMessage::of(stored))?);
    Ok(())
}

/// A request: placed in the payer's request record with the cancellation kept for it, if
/// any, in the effect's batch, which also drops the kept copy (Paykit spec 8.1).
fn place_request(
    cx: &mut ReceiveContext<'_>,
    target: &ActivityTarget,
    payee: [u8; 32],
    stored: &StoredMessage,
) -> Result<(), Fail> {
    let own = cx.identity();
    let kept_key = key::kept_cancellation(&cx.record_key(&target.record_target())?);
    let Some(bytes) = cx.local_get(kept_key.as_bytes())? else {
        return place(cx, target, payee, Role::Request, stored);
    };
    // A kept cancellation that no longer reads leaves the request unopened rather than
    // payable.
    let cancellation = state::decode::<KeptMessage>(&bytes)
        .and_then(|kept| kept.stored(&own))
        .map_err(|_| Fail::Drop("a kept cancellation doesn't read".into()))?;
    let current = held_value(cx, target)?;
    let mut record = activity::read(current.as_deref(), &own, ActivitySide::Payer)
        .unwrap_or_else(|| activity::empty(ActivitySide::Payer, payee));
    let placed = activity::place(&mut record, Role::Request, stored.clone());
    let cancelled = activity::place(&mut record, Role::Cancellation, cancellation);
    if placed || cancelled {
        cx.put_record(&target.record_target(), &record.to_json()?, Vec::new())?;
    }
    cx.local(LocalWrite::Delete {
        key: kept_key.into_bytes(),
    });
    Ok(())
}

/// A payer's acceptance, rejection, cancellation or proof, at the payee (Paykit spec 3.4,
/// 6, 8.4). Returns whether it is about a record of this app's: a request it holds, or a
/// payment without a request.
fn payer_reply(
    cx: &mut ReceiveContext<'_>,
    stored: &StoredMessage,
    scopes: &[String],
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
            // The request's record goes in the batch that queues the request, so it comes
            // before any reply in the feed: a reply to a request this app holds no record of
            // isn't about one of its scopes' requests.
            if held_value(cx, &target)?.is_none() {
                return Ok(false);
            }
            place(cx, &target, sender, role, stored)?;
            Ok(true)
        }
        None => {
            // A payment without a request: keyed by its settlement, so every proof of one
            // settlement names one record and is credited once (Paykit spec 8.4). A proof
            // whose method gives no settlement ID isn't credited.
            let Body::PaymentProof(proof) = stored.body() else {
                return Ok(false);
            };
            let Some(settlement) = proof.settlement_id() else {
                return Ok(false);
            };
            let target = ActivityTarget::payee(
                natural_key::payee_payment_record(&proof.payment_endpoint_identifier, &settlement),
                None,
            );
            place(cx, &target, sender, role, stored)?;
            Ok(true)
        }
    }
}

/// A receipt (Paykit spec 7.3, 8.4): stored in the payer's record and, in the same batch,
/// the attempt it names settled by evidence when it is open, unknown or failed and the
/// claim isn't SETTLED (Switch spec 10.2 row 3).
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
    let Some(value) = held_value(cx, &target)? else {
        return Ok(());
    };
    let Some(record) = activity::read(Some(&value), &own, target.side.side()) else {
        return Ok(());
    };
    if record.counterparty != sender {
        return Ok(());
    }
    place(cx, &target, sender, Role::Receipt, stored)?;
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
            // A kept entry that doesn't read holds the receipt back rather than settling the
            // claim with no deadline (Switch spec 8.2): the store is corrupt.
            let journaled = match cx.local_get(key::journal(&attempt_id).as_bytes())? {
                Some(bytes) => Some(
                    state::decode::<JournalEntry>(&bytes)
                        .and_then(|entry| entry.check().map(|()| entry))
                        .map_err(|_| Fail::NotNow)?,
                ),
                None => None,
            };
            let deadline = journaled.map_or(ObligationDeadline::NotKnown, |e| {
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
    let Some(view) = claim_view_or_need(cx, &obligation)? else {
        return Ok(());
    };
    if view.status == ClaimStatus::Settled {
        return Ok(());
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
    }
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

/// The view of `claim_key` the app keeps, or the switch's answer read for this step;
/// [`Fail::Need`] when neither is there yet, and `None` when the switch holds no such claim
/// or refused the read.
pub(crate) fn claim_view_or_need(
    cx: &ReceiveContext<'_>,
    claim_key: &[u8; 32],
) -> Result<Option<ClaimView>, Fail> {
    if let Some(view) = cx.claim_view(claim_key)? {
        return Ok(Some(view));
    }
    match cx.switch_claim(claim_key) {
        None => Err(Fail::Need(vec![Need::Claim(*claim_key)])),
        Some(ClaimFetch::Read(read)) => Ok(Some(read.view.clone())),
        Some(_) => Ok(None),
    }
}

/// A Private Payment List (Paykit spec 5, 8.4; Paykit wire 5.4): the latest list of its
/// publishing app replaces an older one, keeping the single-use endpoints marked used.
fn list_effect(cx: &mut ReceiveContext<'_>, stored: &StoredMessage) -> Result<(), Fail> {
    let payee = stored.sender();
    let app_id = stored.message().from.app_id;
    let Body::PrivatePaymentList(list) = stored.body() else {
        return Ok(());
    };
    let own = cx.identity();
    let target = list_target(&payee, &app_id);
    let held = cx
        .record(&target)?
        .and_then(|r| r.content.value().map(<[u8]>::to_vec))
        .and_then(|v| PaymentListRecord::read(&v, &own).ok());
    let msg_id = &stored.message().msg_id;
    let mut record = match held {
        Some(held) => {
            if held.list.message().msg_id == *msg_id || held.msg_ids.contains(msg_id) {
                return Ok(());
            }
            let held_version = match held.list.body() {
                Body::PrivatePaymentList(l) => l.version,
                _ => 0,
            };
            // A list with a lower or equal version than one held is ignored (Paykit wire
            // 4.13).
            if list.version <= held_version {
                return Ok(());
            }
            held
        }
        None => PaymentListRecord {
            payee,
            app_id,
            list: stored.clone(),
            used: Vec::new(),
            msg_ids: Vec::new(),
        },
    };
    record.list = stored.clone();
    record.msg_ids =
        paykit_lib::records::bounded_msg_ids(std::slice::from_ref(msg_id), &record.msg_ids);
    cx.put_record(&target, &record.to_json()?, Vec::new())?;
    Ok(())
}

impl Paykit {
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

/// The `unrequested` member of a payer's record of a payment without a request.
pub(crate) fn unrequested_of(
    destination: &crate::journal::Destination,
) -> crate::error::Result<UnrequestedDestination> {
    Ok(UnrequestedDestination {
        app_id: destination.app_id,
        endpoint_id: destination.endpoint_id()?,
        payload: destination.payload()?,
        source: destination.source()?,
        list_version: destination.list_version,
        single_use: destination.single_use,
    })
}
