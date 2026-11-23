//! The counterparty send hold at each delivery attempt (Paykit spec 8.1, "Blocked
//! counterparties", "Messages already queued").
//!
//! The client asks [`crate::PaykitProtocol`] before every delivery or re-send of an
//! outbox message: direct, flushed, resumed, from a kept batch, or carried by reload or
//! restore. A message the hold governs goes only while the current deny decision
//! permits it, so one queued or signed before a block waits once the block is recorded.
//! An exception goes from the signed message and the durable state that establishes its
//! role, never from its kind alone: a payee's cancellation by its kept side, a payment's
//! acceptance by the evidence written before it was queued. An exception covers the first
//! delivery and its retries only: a re-send answering a `resend_request` goes only while
//! the deny decision permits it, whatever its kind (Paykit spec 9).

use std::collections::BTreeMap;

use paykit_lib::kinds::Kind;
use pubky_switch_client::app::{
    AppError, AppEvent, DeliveryOrigin, DeliveryView, KeptBatchStatus, QueuedMessage,
    SENT_RETENTION_MS,
};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::records::CLOCK_TOLERANCE_MS;
use serde::{Deserialize, Serialize};

use crate::activity::{KeptSide, SideTag};
use crate::error::{PaykitError, Result};
use crate::hold::AnswerRecord;
use crate::notices::{GivenUpFrom, GivenUpOrigin, KeptNotice};
use crate::paykit::Paykit;
use crate::receive;
use crate::state::{self, key, Tx};

/// Why an outgoing acceptance may go while sends to its payee are held, kept at
/// `ex/<msg_id>` before or with the batch that queues it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendExemption {
    pub(crate) msg_id: String,
    pub(crate) reason: ExemptionReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ExemptionReason {
    /// The acceptance reports a payment the payment method was already asked to make.
    DispatchedPayment,
    /// The user confirmed accepting this subscription of a blocked payee. Only its
    /// original delivery and retries of it go, never a re-send or a copy reconstructed
    /// from a shared record.
    ConfirmedWhileBlocked {
        #[serde(with = "serde_b64")]
        payee: [u8; 32],
        #[serde(with = "serde_b64")]
        request_hash: [u8; 32],
    },
}

impl SendExemption {
    pub(crate) fn dispatched(msg_id: &str) -> Self {
        Self {
            msg_id: msg_id.to_owned(),
            reason: ExemptionReason::DispatchedPayment,
        }
    }

    pub(crate) fn confirmed_while_blocked(
        msg_id: &str,
        payee: [u8; 32],
        request_hash: [u8; 32],
    ) -> Self {
        Self {
            msg_id: msg_id.to_owned(),
            reason: ExemptionReason::ConfirmedWhileBlocked {
                payee,
                request_hash,
            },
        }
    }
}

/// The deny decision for the recipient, as the app's own block, unblock and copy of the
/// deny list give it (Paykit spec 8.1, "How fresh the deny list is").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DenyDecision {
    Permits,
    Blocked,
    /// The copy is older than its refresh interval, never read, or an own change is
    /// undecided: the send waits for a read of the lists.
    Unknown,
}

pub(crate) fn deny_decision(delivery: &DeliveryView<'_>) -> Result<DenyDecision> {
    let read = state::delivery_get::<u64>(delivery, key::LISTS_READ)?;
    if !read.is_some_and(|at| receive::lists_read_is_fresh(at, delivery.now_ms())) {
        return Ok(DenyDecision::Unknown);
    }
    let Some(denied) = state::delivery_get::<Vec<[u8; 32]>>(delivery, key::DENIED)? else {
        return Ok(DenyDecision::Unknown);
    };
    let to = &delivery.message().to;
    if let Some(mark) = state::delivery_get::<state::OwnBlockMark>(delivery, &key::own_block(to))? {
        let kept = match mark.unblock_batch() {
            Some(batch) => matches!(
                delivery.kept_batch(batch)?,
                Some(KeptBatchStatus::Kept { .. })
            ),
            None => false,
        };
        return Ok(match mark.counts(kept) {
            Some(false) => DenyDecision::Permits,
            Some(true) => DenyDecision::Blocked,
            None => DenyDecision::Unknown,
        });
    }
    Ok(if denied.contains(to) {
        DenyDecision::Blocked
    } else {
        DenyDecision::Permits
    })
}

/// Whether a message of `kind` that the counterparty hold governs may go now. Every
/// delivery needs a certificate Paykit takes that still sends the kind, an exception
/// included: the exceptions bypass the send hold only (Paykit spec 3.1, 8.1, "Messages
/// already queued"). An app whose certificate doesn't read the lists can never decide that
/// the counterparty isn't blocked, so it holds every message the hold governs; the
/// exceptions still go, under a certificate that sends them, which reads the lists.
///
/// What ends a message held here:
/// - no held kind is sent past its window ([`held_past_window`]); a rejection's or a
///   payer's cancellation's outbox copy is then ended by the receive step, which tells the
///   host ([`Paykit::give_up_expired_stops`], Paykit spec 8.1). Both judge the window on
///   the service time verified, the device clock only while none is known;
/// - a re-send ends at the client's replay deadline, its first acceptance plus 30 days
///   (Switch spec 11);
/// - a request, period details, a list request and a subscription acceptance not
///   confirmed while blocked are never sent past the same window either: past it, the
///   copy stays queued and never goes, and the host gets no notice of it yet. Its record
///   keeps its effect (Paykit spec 8.1): a subscription it accepted stays accepted, and
///   its periods may still be paid, each payment reported by its proof.
pub(crate) fn may_deliver(delivery: &DeliveryView<'_>, kind: Kind) -> Result<bool> {
    let cert = delivery.cert();
    if paykit_lib::scopes::check_certificate(cert).is_err()
        || paykit_lib::kinds::check_sender_scopes(delivery.message(), cert.scopes()).is_err()
    {
        return Ok(false);
    }
    let reads_lists = receive::reads_lists(cert.scopes());
    // Re-sending an answer requires `paykit_contacts` and a known deny copy that permits
    // the counterparty, what settles a payment included (Paykit spec 3.1, 8.1, 9).
    if delivery.resend() {
        return Ok(reads_lists && deny_decision(delivery)? == DenyDecision::Permits);
    }
    let message = delivery.message();
    match kind {
        // A proof and a receipt report a payment already made (Paykit spec 8.1).
        Kind::PaymentProof | Kind::PaymentReceipt => return Ok(true),
        Kind::PaymentRequestCancellation => {
            let side = state::delivery_get::<KeptSide>(delivery, &key::side(&message.msg_id))?;
            if side.is_some_and(|kept| kept.side == SideTag::Payee) {
                return Ok(true);
            }
        }
        Kind::PaymentRequestAcceptance => {
            let exemption = state::delivery_get::<SendExemption>(
                delivery,
                &key::send_exemption(&message.msg_id),
            )?;
            match exemption.map(|e| e.reason) {
                Some(ExemptionReason::DispatchedPayment) => return Ok(true),
                Some(ExemptionReason::ConfirmedWhileBlocked {
                    payee,
                    request_hash,
                }) if payee == message.to
                    && message.refs.as_slice() == [request_hash]
                    && delivery.origin() == DeliveryOrigin::Original =>
                {
                    return Ok(deny_decision(delivery)? != DenyDecision::Unknown);
                }
                _ => {}
            }
        }
        _ => {}
    }
    // A certificate that sends it holds `paykit_contacts` (Paykit spec 3.1, checked
    // above), so its deny decision is never taken without the lists. Its window is
    // judged for every held kind before this ([`held_past_window`]).
    Ok(deny_decision(delivery)? == DenyDecision::Permits)
}

/// The service time a Paykit delivery operation observed from a response it got, shared in
/// memory between [`Paykit`] and its protocol's delivery hook and never stored: a message
/// the counterparty hold governs goes only inside such an operation, on the time it observed
/// (Paykit spec 8.1, "Messages already queued"). A flush no Paykit operation started, by
/// another protocol or through the client directly, observed nothing, so such a message
/// waits, kept, for the next one.
#[derive(Debug, Default)]
pub(crate) struct DeliveryClock {
    state: std::sync::Mutex<ClockState>,
}

#[derive(Debug, Default)]
struct ClockState {
    /// The operations begun so far, which names each.
    begun: u64,
    /// The operation under way, if any.
    current: Option<u64>,
    /// The operation that observed a service time, and that time.
    observed: Option<(u64, u64)>,
}

impl DeliveryClock {
    /// Begin an operation unless one is under way. Whether this call began it.
    fn begin(&self) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.current.is_some() {
            return false;
        }
        state.begun = state.begun.saturating_add(1);
        state.current = Some(state.begun);
        true
    }

    fn end(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.current = None;
            state.observed = None;
        }
    }

    /// Keep `at_ms`, a service time from a response, as the current operation's.
    pub(crate) fn observe(&self, at_ms: u64) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(op) = state.current {
                let at_ms = match state.observed {
                    Some((seen, earlier)) if seen == op => earlier.max(at_ms),
                    _ => at_ms,
                };
                state.observed = Some((op, at_ms));
            }
        }
    }

    /// The service time the current operation observed; `None` outside an operation, or
    /// before it observed one, which holds every governed message.
    pub(crate) fn observed(&self) -> Option<u64> {
        let state = self.state.lock().ok()?;
        match (state.current, state.observed) {
            (Some(op), Some((seen, at_ms))) if op == seen => Some(at_ms),
            _ => None,
        }
    }
}

/// A Paykit delivery operation under way ([`DeliveryClock`]); it ends when dropped, on every
/// path out of the call that began it. One begun inside another is the outer one's.
pub(crate) struct DeliveryOperation {
    protocol: std::sync::Arc<crate::protocol::PaykitProtocol>,
    began: bool,
}

impl Drop for DeliveryOperation {
    fn drop(&mut self) {
        if self.began {
            self.protocol.delivery.end();
        }
    }
}

/// Whether `delivery` is the first delivery, or a retry of it, of a message the
/// counterparty hold governs that is past its window: 30 days past its `created_at`, less
/// the clock tolerance, by the highest service time the app verified that its state holds
/// (written before every delivery Paykit starts), and by the device clock only while it
/// holds none, so neither a slow device clock sends late nor a fast one ends a message
/// early. Such a message is never sent, whatever its kind and its other rules (Paykit spec
/// 8.1, "Messages already queued"), a subscription acceptance the user confirmed while its
/// payee was blocked included. What still goes when first sent while sends are held keeps
/// the client's own deadline instead, as the hold doesn't govern it: a payment's proof, a
/// receipt, a payee's cancellation, the acceptance of a dispatched payment, the user's
/// `allowance_end`, a list's withdrawal and an own app's `executor_notice`. A re-send keeps
/// the client's replay deadline from its first acceptance (Switch spec 11).
///
/// Not every window has this rule: the client judges one it queues again from signed
/// plaintext (`App::queue_stored`), which ends a held answer released from its record
/// ([`Paykit::queue_own_again`]), by the later of the device clock and the service time,
/// as `window_now` mirrors. Under a fast device clock such an answer can be given up while
/// a stop already queued still goes.
pub(crate) fn held_past_window(delivery: &DeliveryView<'_>, observed: Option<u64>) -> Result<bool> {
    if delivery.resend() {
        return Ok(false);
    }
    let message = delivery.message();
    let goes = match paykit_lib::kinds::MessageKind::parse(&message.kind) {
        Ok(paykit_lib::kinds::MessageKind::Paykit(kind)) => match kind {
            Kind::PaymentProof | Kind::PaymentReceipt | Kind::ExecutorNotice => true,
            Kind::PaymentRequestCancellation => {
                state::delivery_get::<KeptSide>(delivery, &key::side(&message.msg_id))?
                    .is_some_and(|kept| kept.side == SideTag::Payee)
            }
            Kind::PaymentRequestAcceptance => state::delivery_get::<SendExemption>(
                delivery,
                &key::send_exemption(&message.msg_id),
            )?
            .is_some_and(|e| e.reason == ExemptionReason::DispatchedPayment),
            Kind::AllowanceEnd => crate::allowances::user_end(delivery)?,
            Kind::PrivatePaymentList => {
                paykit_lib::bodies::PrivatePaymentList::from_body(&message.body)
                    .is_ok_and(|list| list.payment_endpoints.is_empty())
            }
            _ => false,
        },
        _ => false,
    };
    if goes {
        return Ok(false);
    }
    // Governed, it goes only on a service time its delivery operation observed, never
    // below the one kept (Paykit spec 8.1).
    let Some(observed) = observed else {
        return Ok(true);
    };
    let kept = state::delivery_get::<u64>(delivery, key::SERVICE_TIME)?.unwrap_or(0);
    Ok(past_replay_window(
        message.created_at_ms,
        observed.max(kept),
    ))
}

/// Whether a message signed at `created_at_ms` is past its 30 days, less the clock
/// tolerance, at `now_ms`: held until then, it is never sent (Paykit spec 8.1, "Blocked
/// counterparties", "Messages already queued").
pub(crate) fn past_replay_window(created_at_ms: u64, now_ms: u64) -> bool {
    now_ms
        > created_at_ms
            .saturating_sub(CLOCK_TOLERANCE_MS)
            .saturating_add(SENT_RETENTION_MS)
}

/// A rejection or payer's cancellation this app queued, kept at `qs/<msg_id>` in the
/// commit that queues it, for as long as its outbox copy is owed: what the receive step
/// needs to end that copy past its 30 days and tell the host, which the outbox doesn't
/// show (Paykit spec 8.1). `given_up` is set before the client ends the copy, with whether
/// the host is told. The entry stays, a backup and restore included, until neither the
/// outbox nor an unacknowledged report names its message and, once told, the host has
/// acknowledged its given-up notice, so a report a restore carries is taken again as it
/// was decided, and acknowledged (Paykit spec 9), and an in-place restore keeps that
/// notice with it. `told` is set in the commit that takes the report, or that raises the
/// notice of a held answer the client won't queue, and restored with the entry, so a
/// report taken again, after a restore in place included, tells the host nothing twice
/// (Paykit spec 8.1).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueuedStop {
    pub(crate) msg_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) payee: [u8; 32],
    #[serde(with = "serde_b64")]
    pub(crate) request_hash: [u8; 32],
    #[serde(with = "serde_b64")]
    pub(crate) hash: [u8; 32],
    pub(crate) created_at_ms: u64,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) given_up: Option<GivenUp>,
    pub(crate) told: bool,
}

/// How a stop's outbox copy past its window is ended, by what its request record says of
/// it ([`crate::hold::AnswerRecord`], Paykit spec 8.1, "Undelivered messages").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GivenUp {
    /// The record holds the stop, or lost it with a write: the host is told.
    Told,
    /// The record is gone, or holds another stop that covers it: nothing is told of it
    /// as given up, though a possible delivery still is.
    Silent,
}

impl QueuedStop {
    /// How far its end has gone: undecided, decided, then told.
    pub(crate) fn progress(&self) -> (bool, bool) {
        (self.given_up.is_some(), self.told)
    }

    /// The entry of `signed`, a stop answering the request `request_hash`, just signed.
    pub(crate) fn signed(
        signed: &pubky_switch_client::app::SignedMessage,
        request_hash: [u8; 32],
    ) -> Result<Self> {
        let message = pubky_switch_core::message::Message::parse(
            signed.plaintext().json(),
            paykit_lib::kinds::OPAQUE_MEMBERS,
        )?;
        Ok(Self {
            msg_id: message.msg_id,
            payee: message.to,
            request_hash,
            hash: signed.hash(),
            created_at_ms: message.created_at_ms,
            given_up: None,
            told: false,
        })
    }

    /// The entry of `stored`, a stop answering the request `request_hash`, queued again
    /// from its record.
    pub(crate) fn stored(
        stored: &paykit_lib::records::StoredMessage,
        request_hash: [u8; 32],
    ) -> Self {
        let message = stored.message();
        Self {
            msg_id: message.msg_id.clone(),
            payee: message.to,
            request_hash,
            hash: stored.hash(),
            created_at_ms: message.created_at_ms,
            given_up: None,
            told: false,
        }
    }
}

impl Paykit {
    /// Keep the highest service time the app verified where the delivery hook reads it
    /// (`sv`), so a held message's window is judged by it ([`held_past_window`]). It only
    /// goes up.
    pub(crate) async fn note_service_time(&mut self) -> Result<()> {
        let mut tx = Tx::default();
        self.put_service_time(&mut tx).await?;
        self.local_commit(tx).await
    }

    /// Put in `tx` the highest service time verified as `sv`, when it is higher than the one
    /// kept: the one writer of `sv`.
    pub(crate) async fn put_service_time(&self, tx: &mut Tx) -> Result<()> {
        let Some(at) = self.service_time() else {
            return Ok(());
        };
        if self
            .local_get::<u64>(key::SERVICE_TIME)
            .await?
            .is_some_and(|kept| kept >= at)
        {
            return Ok(());
        }
        tx.put(key::SERVICE_TIME, &at)?;
        Ok(())
    }

    /// Begin a delivery operation, unless one is under way ([`DeliveryClock`]).
    pub(crate) fn delivery_operation(&self) -> DeliveryOperation {
        DeliveryOperation {
            protocol: std::sync::Arc::clone(&self.protocol),
            began: self.protocol.delivery.begin(),
        }
    }

    /// The client's owed deliveries of Paykit's kinds, with the protocol's verdict on each as
    /// a delivery operation that observed the highest service time verified gives it, with
    /// no read: test builds only.
    ///
    /// # Errors
    ///
    /// The client's errors, as for `App::queued_messages`.
    #[cfg(feature = "test-support")]
    pub async fn queued_in_delivery(
        &self,
    ) -> std::result::Result<Vec<QueuedMessage>, pubky_switch_client::app::AppError> {
        let _delivery = self.delivery_operation();
        self.observe_service_time();
        self.app.queued_messages(crate::protocol::BACKUP_NAME).await
    }

    /// Try the queued message `msg_id` once, as `App::flush_message` does, inside a Paykit
    /// delivery operation, which observes a service time first when a held message waits:
    /// test builds only.
    ///
    /// # Errors
    ///
    /// The client's errors.
    #[cfg(feature = "test-support")]
    pub async fn flush_message_in_delivery(
        &mut self,
        msg_id: &str,
    ) -> std::result::Result<
        Option<pubky_switch_client::app::Delivery>,
        pubky_switch_client::app::AppError,
    > {
        self.send_message(msg_id).await
    }

    /// Try every queued message once, as `App::flush_outbox` does, inside a Paykit delivery
    /// operation: test builds only.
    ///
    /// # Errors
    ///
    /// The client's errors.
    #[cfg(feature = "test-support")]
    pub async fn flush_outbox_in_delivery(
        &mut self,
    ) -> std::result::Result<
        pubky_switch_client::app::OutboxReport,
        pubky_switch_client::app::AppError,
    > {
        self.send_outbox().await
    }

    /// Keep the highest service time verified, which a response just brought, as the current
    /// delivery operation's.
    pub(crate) fn observe_service_time(&self) {
        if let Some(at) = self.service_time() {
            self.protocol.delivery.observe(at);
        }
    }

    /// Make the current delivery operation observe a service time before a held message
    /// can go, when it hasn't yet and one waits: one read of the lists, which a response
    /// answers (Paykit spec 8.1). A read that fails observes nothing, and such messages
    /// wait for the next operation.
    pub(crate) async fn observe_for_delivery(&mut self) -> Result<()> {
        if self.protocol.delivery.observed().is_some() {
            return Ok(());
        }
        // A listing the client can't give now observes nothing: what waits keeps waiting.
        let waiting = self
            .app
            .queued_messages(crate::protocol::BACKUP_NAME)
            .await
            .is_ok_and(|queued| {
                queued
                    .iter()
                    .any(|queued| !queued.resend && !queued.may_send)
            });
        if !waiting
            || self.certificate_refused().is_some()
            || !receive::reads_lists(self.app.cert().scopes())
        {
            return Ok(());
        }
        // The lists read is the cheap authenticated read: its answer is observed whatever
        // becomes of the copy it carries, which a kept floor write leaves as it is.
        let now = self.now();
        match self.app.allow_list().await {
            Ok(lists) => {
                self.observe_service_time();
                if !Box::pin(self.floor_write_is_kept()).await? {
                    match Box::pin(self.record_lists(lists, now)).await {
                        Ok(_) => {}
                        Err(
                            error @ PaykitError::Client(AppError::Alarm(_) | AppError::Store(_)),
                        ) => return Err(error),
                        // Any other failure, such as an unreadable floor or a refusal,
                        // leaves the lists' read time as it was, so a governed message
                        // waits on the deny decision as before (Paykit spec 8.1). The
                        // receive step's own read reports it.
                        Err(_) => {}
                    }
                }
            }
            Err(error @ (AppError::Alarm(_) | AppError::Store(_))) => return Err(error.into()),
            // A read that fails observes nothing, and leaves the copy of the lists not
            // known, as any failed read of them does (Paykit wire 4.1).
            Err(_) => self.local_delete(key::LISTS_READ).await?,
        }
        Ok(())
    }

    /// End the outbox copies of this app's rejections and payer's cancellations that are
    /// past 30 days from their `created_at`, less the clock tolerance, by the service time
    /// verified, or the device clock while none is: such a stop is never sent, and a client
    /// original carries no deadline of its own. As for one held in its record
    /// ([`Paykit::queue_own_again`]), it is decided at the first receive past that time at
    /// which the copy of the deny list is known and doesn't hold the payee, with the records
    /// listed and no scope key awaited (Paykit spec 8.1, "Blocked counterparties"), on what
    /// its request record says of it ([`Paykit::answer_record`]): nothing is decided while
    /// the record doesn't read or is paused as divergent; the host is told it was given up
    /// when the record holds the stop or lost it with a write, and not when the record is
    /// gone or another stop covers it, since that notice requires a record that holds the
    /// message ("Undelivered messages"); a possible delivery is always told. Each is ended
    /// through the client's
    /// withdrawal, which reports it undelivered when no try of it can have reached the
    /// payee's switch, and unknown otherwise; that report is taken at once, the host told
    /// in the commit that takes it ([`crate::NoticeKind::MessageGivenUp`] or
    /// [`crate::NoticeKind::PossiblyDelivered`]). An entry whose copy the outbox no longer
    /// holds, delivered, refused or ended otherwise, goes.
    pub(crate) async fn give_up_expired_stops(&mut self) -> Result<()> {
        let kept = self.local_scan::<QueuedStop>(key::QUEUED_STOPS).await?;
        if kept.is_empty() {
            return Ok(());
        }
        let queued: BTreeMap<String, QueuedMessage> = self
            .app
            .queued_messages(crate::protocol::BACKUP_NAME)
            .await?
            .into_iter()
            .filter(|queued| !queued.resend)
            .map(|queued| (queued.msg_id.clone(), queued))
            .collect();
        // The window's clock, as the delivery hook judges it ([`held_past_window`]).
        let now = self.service_time().unwrap_or_else(|| self.now());
        let mut reported: Option<std::collections::BTreeSet<String>> = None;
        let mut gone = Tx::default();
        let mut expired = Vec::new();
        for (_, stop) in kept {
            let at = key::queued_stop(&stop.msg_id);
            let owed = queued
                .get(&stop.msg_id)
                .is_some_and(|copy| copy.hash == stop.hash && copy.to == stop.payee);
            if !owed {
                // One told keeps its entry while the host hasn't acknowledged its notice,
                // so an in-place restore keeps that notice with it (Paykit spec 8.1,
                // "Undelivered messages").
                if stop.told
                    && self
                        .local_get::<KeptNotice>(&key::message_notice(&stop.payee, &stop.msg_id))
                        .await?
                        .is_some()
                {
                    continue;
                }
                // One given up keeps its entry while a report of its end isn't
                // acknowledged, which is taken, or taken again after a restore, as it was
                // decided.
                if stop.given_up.is_some() {
                    if reported.is_none() {
                        reported = Some(self.reported_ends().await?);
                    }
                    if reported
                        .as_ref()
                        .is_some_and(|ids| ids.contains(&stop.msg_id))
                    {
                        continue;
                    }
                }
                gone.delete(&at);
                continue;
            }
            if past_replay_window(stop.created_at_ms, now)
                && !self.delivery_held(&stop.payee).await?
            {
                expired.push(stop);
            }
        }
        self.local_commit(gone).await?;
        for mut stop in expired {
            let at = key::queued_stop(&stop.msg_id);
            stop.given_up =
                match Box::pin(self.answer_record(&stop.payee, &stop.request_hash, &stop.msg_id))
                    .await?
                {
                    // Nothing is decided, and no decision left by an end that didn't happen
                    // applies to a report the host causes meanwhile.
                    // One told keeps its decision: its notice is out, and a report of its
                    // end is still taken by it.
                    AnswerRecord::Undecided => {
                        if !stop.told && stop.given_up.take().is_some() {
                            self.local_put(&at, &stop).await?;
                        }
                        continue;
                    }
                    AnswerRecord::Holds | AnswerRecord::Lacks => Some(GivenUp::Told),
                    AnswerRecord::Gone | AnswerRecord::Covered => Some(GivenUp::Silent),
                };
            self.local_put(&at, &stop).await?;
            if self
                .app
                .withdraw_message(crate::protocol::BACKUP_NAME, &stop.msg_id)
                .await?
            {
                self.take_delivery_duty(&stop.msg_id).await?;
            } else {
                self.local_delete(&at).await?;
            }
        }
        Ok(())
    }

    /// The messages Paykit owns whose end a report the host hasn't acknowledged names.
    async fn reported_ends(&mut self) -> Result<std::collections::BTreeSet<String>> {
        Ok(self
            .app
            .pending_duty_events()
            .await?
            .into_iter()
            .filter(|duty| {
                duty.owners
                    .iter()
                    .any(|owner| owner == crate::protocol::BACKUP_NAME)
            })
            .filter_map(|duty| match duty.event {
                AppEvent::MessageUndelivered { msg_id, .. }
                | AppEvent::DeliveryUnknown { msg_id, .. } => Some(msg_id),
                _ => None,
            })
            .collect())
    }

    /// Take, in `tx`, the client's report that the outbox copy of a stop ended past its 30
    /// days ([`Paykit::give_up_expired_stops`]): the host is told it was given up when no
    /// try of it can have reached the payee's switch, unless its record left nothing to tell
    /// of, and always that it may have been delivered otherwise. Whether `event` is such a
    /// report: one about a stop the receive step didn't give up stays for the host.
    pub(crate) async fn take_expired_stop(&self, tx: &mut Tx, event: &AppEvent) -> Result<bool> {
        let (msg_id, undelivered) = match event {
            AppEvent::MessageUndelivered {
                msg_id,
                resend: false,
                ..
            } => (msg_id, true),
            AppEvent::DeliveryUnknown { msg_id, .. } => (msg_id, false),
            _ => return Ok(false),
        };
        let at = key::queued_stop(msg_id);
        let Some((mut stop, given_up)) = self
            .local_get::<QueuedStop>(&at)
            .await?
            .and_then(|stop| stop.given_up.map(|given_up| (stop, given_up)))
        else {
            return Ok(false);
        };
        // Told once: a report taken again tells nothing more.
        if stop.told {
            return Ok(true);
        }
        stop.told = true;
        tx.put(&at, &stop)?;
        // Only a stop never delivered goes untold once its record is gone or covered: one
        // that may have been delivered is always told, with or without a record (Paykit spec
        // 8.1, "Undelivered messages"; 9).
        if undelivered && given_up == GivenUp::Silent {
            return Ok(true);
        }
        let now = self.service_time().unwrap_or(0);
        let (notice_key, notice) = if undelivered {
            (
                key::message_notice(&stop.payee, &stop.msg_id),
                KeptNotice::given_up(
                    stop.payee,
                    &stop.msg_id,
                    Some(stop.request_hash),
                    now,
                    GivenUpFrom::Origin(GivenUpOrigin::HeldAnswer),
                ),
            )
        } else {
            (
                key::restored_delivery(&stop.payee, &stop.msg_id, &stop.hash),
                KeptNotice::delivery_unknown(&stop.payee, &stop.msg_id, &stop.hash, now),
            )
        };
        if self.kept_notice(&notice_key).await?.is_none() {
            tx.put(&notice_key, &notice)?;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_send_exemption_current_structure() {
        let confirmed = SendExemption::confirmed_while_blocked(
            "123e4567-e89b-42d3-a456-426614174000",
            [1; 32],
            [2; 32],
        );
        for value in [
            SendExemption::dispatched("123e4567-e89b-42d3-a456-426614174000"),
            confirmed,
        ] {
            let json = serde_json::to_value(&value).unwrap();
            assert_eq!(
                serde_json::from_value::<SendExemption>(json.clone()).unwrap(),
                value
            );
            let mut extra = json.clone();
            extra
                .as_object_mut()
                .unwrap()
                .insert("extra".into(), serde_json::json!(1));
            assert!(serde_json::from_value::<SendExemption>(extra).is_err());
        }
    }

    #[test]
    fn test_send_holds_queued_stop_names_its_decision() {
        let stop = QueuedStop {
            msg_id: "123e4567-e89b-42d3-a456-426614174000".into(),
            payee: [1; 32],
            request_hash: [2; 32],
            hash: [3; 32],
            created_at_ms: 4,
            given_up: Some(GivenUp::Silent),
            told: true,
        };
        let json = serde_json::to_value(&stop).unwrap();
        assert_eq!(
            serde_json::from_value::<QueuedStop>(json.clone()).unwrap(),
            stop
        );
        // A decision is always written, `null` for none: one missing is refused.
        let mut missing = json.clone();
        missing.as_object_mut().unwrap().remove("given_up");
        assert!(serde_json::from_value::<QueuedStop>(missing).is_err());
        let mut undecided = json;
        undecided["given_up"] = serde_json::Value::Null;
        assert_eq!(
            serde_json::from_value::<QueuedStop>(undecided)
                .unwrap()
                .given_up,
            None
        );
    }
}
