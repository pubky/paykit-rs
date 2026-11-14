//! Re-sends (Paykit spec 9, Paykit wire 4.12).
//!
//! - **Answering** a `resend_request`, kept by the receive step with the cursor move and
//!   answered by [`crate::Paykit::receive`]: the sender re-sends, from the messages it keeps (the
//!   client keeps each for 30 days from its first acceptance, Switch spec 11), those the
//!   request selects, of kinds the requesting app's certificate receives, and a payer's
//!   replies only to an app that takes them under their label (Paykit spec 3.4). It re-sends
//!   one message to one requester identity at most once an hour, and at most a daily number
//!   of messages per requester identity.
//! - **Asking**: entries the app can't read, `missed` items and a provider loss queue
//!   `resend_request`s, which [`crate::Paykit::receive`] sends; the client bounds their times
//!   (Paykit spec 9, "What an app asks for"). The marks of messages dropped before they were
//!   taken ask on their own cadence (`marks.rs`). A `resend_request` adds no conversation
//!   entry.

use paykit_lib::bodies::ResendRequest;
use paykit_lib::kinds::{targets_app, Kind, Side, OPAQUE_MEMBERS};
use paykit_lib::scopes::verify_certificate;
use pubky_switch_client::app::{Delivery, KeptBatchStatus, SENT_RETENTION_MS};
use pubky_switch_core::enc::unb64_array;
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::message::Message;
use pubky_switch_core::records::DAY_MS;
use pubky_switch_core::sig::SignedDoc;

use crate::activity::{KeptSide, SideTag};
use crate::error::{PaykitError, Result};
use crate::marks::QueuedRequest;
use crate::paykit::Paykit;
use crate::protocol::{body_label, holds_role, MissedItemMark, QueuedAsk};
use crate::state::{key, Tx};

/// What became of the request signed for an ask ([`Paykit::queued_ask`]).
enum QueuedAskState {
    /// In a batch the client keeps, to send again.
    Waiting,
    /// In the outbox, or delivered.
    Queued,
    /// Never queued, refused for good, or one the outbox no longer knows (after a restore, or
    /// 30 days after its delivery): the ask is asked anew, at most a second request, never
    /// an ask dropped.
    NotQueued,
}

/// One message is re-sent to one requester identity at most once an hour (Paykit wire 4.12).
pub const RESEND_COOLDOWN_MS: u64 = 3_600_000;

/// What to ask a counterparty to re-send (Paykit wire 4.12).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResendSelection {
    /// Messages by `msg_id`.
    pub msg_ids: Vec<String>,
    /// Messages the counterparty's inbox accepted at or after this time.
    pub accepted_from_ms: Option<u64>,
    /// ... and at or before this time.
    pub accepted_until_ms: Option<u64>,
    /// Requests by hash, with every message that names one.
    pub request_hashes: Vec<[u8; 32]>,
}

impl ResendSelection {
    /// Messages by `msg_id`.
    #[must_use]
    pub fn with_msg_ids(mut self, msg_ids: Vec<String>) -> Self {
        self.msg_ids = msg_ids;
        self
    }

    /// Messages the counterparty's inbox accepted at or after this time.
    #[must_use]
    pub fn with_accepted_from_ms(mut self, accepted_from_ms: u64) -> Self {
        self.accepted_from_ms = Some(accepted_from_ms);
        self
    }

    /// Messages the counterparty's inbox accepted at or before this time.
    #[must_use]
    pub fn with_accepted_until_ms(mut self, accepted_until_ms: u64) -> Self {
        self.accepted_until_ms = Some(accepted_until_ms);
        self
    }

    /// Requests by hash, with every message that names one.
    #[must_use]
    pub fn with_request_hashes(mut self, request_hashes: Vec<[u8; 32]>) -> Self {
        self.request_hashes = request_hashes;
        self
    }
}

impl Paykit {
    /// Ask `counterparty` to re-send messages (Paykit spec 9, Paykit wire 4.12). Best effort:
    /// the counterparty can't be made to answer. Returns the request's `msg_id`. Nothing is
    /// queued while sends to `counterparty` are held: while the copy of the deny list holds
    /// it, or, at an app holding `paykit_contacts`, while that copy isn't known, which a read of the
    /// lists is tried for first (Paykit spec 8.1, "Blocked counterparties"). The ask goes in
    /// one batch with the raise of the block floor it needs (`crate::floor`).
    ///
    /// # Errors
    ///
    /// [`crate::Refusal::NotNow`] while sends to `counterparty` are held, or while the app
    /// can't read the block floor it must raise first (Paykit wire 5.6: when `counterparty`
    /// has a live block mark here, or the app owes a raise), with nothing queued;
    /// [`paykit_lib::Error::Validation`] for a selection that selects nothing; the client's
    /// errors, a `409 conflict` on the block floor after it is read again, and the switch's
    /// refusal of the raise that the same request won't change, such as `403
    /// permission_missing`, as it is, with nothing queued. When the client
    /// kept the batch that raises the floor and queues the ask
    /// ([`crate::PaykitError::kept_batch`]), both go, the same bytes, at the app's next write
    /// or read: the call isn't made again.
    pub async fn request_resend(
        &mut self,
        counterparty: [u8; 32],
        selection: ResendSelection,
    ) -> Result<String> {
        self.refresh_own()?;
        self.refuse_while_sends_held(&counterparty).await?;
        let signed = self.resend_request_message(counterparty, selection)?;
        let msg_id = self.queue_resend_request(signed, &counterparty).await?;
        self.app.flush_message(&msg_id).await?;
        Ok(msg_id)
    }

    /// A `resend_request` to `counterparty` for `selection`, signed and not yet queued.
    pub(crate) fn resend_request_message(
        &self,
        counterparty: [u8; 32],
        selection: ResendSelection,
    ) -> Result<pubky_switch_client::app::SignedMessage> {
        let body = ResendRequest {
            msg_ids: selection.msg_ids,
            accepted_from: selection.accepted_from_ms,
            accepted_until: selection.accepted_until_ms,
            request_hashes: selection.request_hashes,
            billing_period: None,
        }
        .to_body()?;
        self.sign(counterparty, Kind::ResendRequest, Vec::new(), body)
    }

    /// Send the re-sends the protocol queued: for entries the app couldn't read, to their
    /// sender; after a provider loss, to every counterparty with Paykit activity; after
    /// `missed` items, to the counterparties [`Paykit::missed_counterparties`] names; after a
    /// restore, to the payees of open requests. Each is an ask kept for its counterparty
    /// (`ask/`), sent and dropped once nothing holds sends to it back
    /// ([`Paykit::sends_held`]): while the counterparty is blocked, or the copy of the deny
    /// list isn't known, the ask stays, and goes once it is unblocked (Paykit spec 8.1,
    /// "Blocked counterparties"), until 30 days after the latest time it asks for, when the
    /// counterparty keeps none of it any longer. Returns how many requests went out.
    pub(crate) async fn send_asks(&mut self) -> Result<u64> {
        self.queue_missed_asks().await?;
        // Each read bound to its key, as a restore reads it (`state::Stored`).
        let asks = self.local_scan::<QueuedAsk>(key::ASKS).await?;
        // A copy of the deny list not known, as after a restore, is read first when there is
        // an ask to send, so the asks a reconcile queues go now; a read that fails leaves
        // them waiting.
        if !asks.is_empty() && !self.deny_copy_known().await? {
            self.refresh_lists_when_due().await?;
        }
        let mut sent = 0u64;
        let service = self.service_time();
        for (rest, mut ask) in asks {
            let ask_key = format!("{}{rest}", key::ASKS);
            // A request signed for this ask before: one the outbox holds or delivered is the
            // ask's, which goes, the request tried now unless sends to its counterparty are
            // held; one still in a batch the client keeps waits for it; one never queued,
            // refused for good, or one the outbox no longer knows (after a restore, or 30
            // days after its delivery) leaves the ask to be asked anew: at most a second
            // request, never an ask dropped.
            if let Some(queued) = &ask.queued {
                match self.queued_ask(&ask.sender, queued).await? {
                    QueuedAskState::Waiting => continue,
                    QueuedAskState::Queued => {
                        let msg_id = queued.msg_id.clone();
                        self.local_delete(&ask_key).await?;
                        if !self.sends_held(&ask.sender).await? {
                            self.app.flush_message(&msg_id).await?;
                            sent = sent.saturating_add(1);
                        }
                        continue;
                    }
                    QueuedAskState::NotQueued => {}
                }
            }
            if self.sends_held(&ask.sender).await? {
                // Held back, an ask stays until its counterparty keeps nothing it asks for:
                // once 30 days have passed since the latest time it names (its `until`, or
                // its `from` for an ask of everything since), on the highest service time
                // verified, it goes unsent. So the asks held for a blocked counterparty are
                // bounded by the `missed` items, losses and unreadable entries of 30 days.
                let latest = ask.until_ms.unwrap_or(ask.from_ms);
                if crate::stored::past_window(latest, service) {
                    self.local_delete(&ask_key).await?;
                }
                continue;
            }
            self.refresh_own()?;
            let signed = self.resend_request_message(
                ask.sender,
                ResendSelection {
                    accepted_from_ms: Some(ask.from_ms),
                    accepted_until_ms: ask.until_ms,
                    ..ResendSelection::default()
                },
            )?;
            let msg_id = signed.msg_id().to_owned();
            let hash = signed.hash();
            // Kept before it is queued: a crash between the two, or a write that queues it
            // with the raise of the block floor and isn't answered, finds the request here
            // at the next pass, which reads what became of it before asking anew.
            ask.queued = Some(QueuedRequest {
                msg_id: msg_id.clone(),
                hash,
                kept_batch: None,
            });
            self.local_put(&ask_key, &ask).await?;
            match self.queue_resend_request(signed, &ask.sender).await {
                Ok(_) => {}
                // The client kept the batch to send again: the raise and the request go
                // together at the app's next write or read. The ask names the batch, so the
                // next pass waits while the client keeps it, and reads the request's
                // delivery once it doesn't.
                Err(error) if error.kept_batch().is_some() => {
                    ask.queued = Some(QueuedRequest {
                        msg_id,
                        hash,
                        kept_batch: error.kept_batch(),
                    });
                    self.local_put(&ask_key, &ask).await?;
                    continue;
                }
                // An ask that must raise the block floor stays while the floor can't be read,
                // the switch refuses the raise, or the raise meets a `409 conflict`, after
                // which the floor was read again: the next pass builds on it (`crate::floor`).
                // Nothing was queued, and the rest of the asks go on.
                Err(error)
                    if crate::floor::floor_write_waits(&error)
                        || error.code() == Some(ErrorCode::Conflict) =>
                {
                    continue
                }
                Err(error) => return Err(error),
            }
            self.local_delete(&ask_key).await?;
            self.app.flush_message(&msg_id).await?;
            sent = sent.saturating_add(1);
        }
        Ok(sent)
    }

    /// What became of `queued`, the request signed for an ask of `sender`: still waiting in
    /// a batch the client keeps; queued in the outbox or delivered, and shown to be that
    /// request; or neither: never queued, refused for good, or no longer known to the outbox
    /// (after a restore, or 30 days after its delivery).
    ///
    /// # Errors
    ///
    /// A store failure; a corrupt store when the message delivered under its `msg_id` is
    /// another one ([`Paykit::delivered_request`]).
    async fn queued_ask(
        &self,
        sender: &[u8; 32],
        queued: &QueuedRequest,
    ) -> Result<QueuedAskState> {
        if let Some(batch) = queued.kept_batch {
            if matches!(
                self.app.kept_batch(batch).await?,
                Some(KeptBatchStatus::Kept { .. })
            ) {
                return Ok(QueuedAskState::Waiting);
            }
        }
        Ok(match self.app.delivery(&queued.msg_id).await? {
            Some(Delivery::Kept { .. }) => QueuedAskState::Waiting,
            Some(Delivery::Queued { .. }) => QueuedAskState::Queued,
            Some(Delivery::Accepted { .. }) => {
                self.delivered_request(sender, queued).await?;
                QueuedAskState::Queued
            }
            _ => QueuedAskState::NotQueued,
        })
    }

    /// Turn the marks of missed entries into asks, one per counterparty (`ask/`).
    ///
    /// - A provider loss's marks (`missed/`) ask every counterparty with Paykit activity from
    ///   their earliest time, and go.
    /// - A `missed` item's mark (`mi/`) asks the counterparties [`Paykit::missed_counterparties`]
    ///   reads from the copy of the records, once the copy is listed and no scope key is
    ///   awaited. Those it can read now are asked now, each once per mark. While a record of
    ///   the copy is sealed under a scope key the app doesn't hold yet, its counterparty
    ///   can't be read: the mark stays, naming the counterparties asked already, and asks
    ///   those that show once the record opens. It goes once no record is sealed, or once
    ///   [`crate::KEPT_MESSAGE_MS`] have passed since its time on the highest service time
    ///   verified, when no counterparty keeps what it would ask for any longer.
    async fn queue_missed_asks(&mut self) -> Result<()> {
        let lost = self.missed_marks(key::MISSED).await?;
        if !lost.is_empty() {
            let peers: Vec<[u8; 32]> = self
                .local_scan::<u64>(key::PEERS)
                .await?
                .into_iter()
                .map(|(rest, _)| unb64_array::<32>(&rest).map_err(|_| PaykitError::corrupt()))
                .collect::<Result<_>>()?;
            let from = lost.iter().map(|(_, from)| *from).min().unwrap_or(0);
            let mut tx = Tx::default();
            for peer in peers {
                self.queue_ask(&mut tx, peer, from).await?;
            }
            for (rest, _) in lost {
                tx.delete(&format!("{}{rest}", key::MISSED));
            }
            self.local_commit(tx).await?;
        }
        let items = self.missed_item_marks().await?;
        // The counterparties come from the copy of the records, so the asks and their marks
        // wait while it may lack records or waits for scope keys, for the receive that
        // completes it.
        if items.is_empty()
            || self.unlisted().await?.is_some()
            || self.scope_keys_awaited().await?.is_some()
        {
            return Ok(());
        }
        let counterparties = self.missed_counterparties().await?;
        let sealed = self.copy_awaits_key().await?;
        let service = self.service_time();
        let mut tx = Tx::default();
        for (mark_key, mut mark) in items {
            let mut asked: std::collections::BTreeSet<[u8; 32]> =
                mark.asked.iter().copied().collect();
            for counterparty in &counterparties {
                if asked.insert(*counterparty) {
                    self.queue_ask(&mut tx, *counterparty, mark.from_ms).await?;
                }
            }
            if sealed && !crate::stored::past_window(mark.from_ms, service) {
                mark.asked = asked.into_iter().collect();
                tx.put(&mark_key, &mark)?;
            } else {
                tx.delete(&mark_key);
            }
        }
        self.local_commit(tx).await
    }

    /// Ask `counterparty`, in `tx`, to send again what its inbox accepted from `from_ms` on
    /// (`ask/<counterparty>/<from>`). An ask kept already at that key, read bound to it, is
    /// widened to everything from then on.
    pub(crate) async fn queue_ask(
        &self,
        tx: &mut Tx,
        counterparty: [u8; 32],
        from_ms: u64,
    ) -> Result<()> {
        let ask_key = key::ask(&counterparty, from_ms);
        // One kept already is read bound to its key: a value that isn't its key's is a
        // corrupt store, never written over.
        self.local_get::<QueuedAsk>(&ask_key).await?;
        tx.put(
            &ask_key,
            &QueuedAsk {
                sender: counterparty,
                from_ms,
                until_ms: None,
                queued: None,
            },
        )?;
        Ok(())
    }

    /// The marks of missed entries under `prefix` (`missed/`), each read bound to its key,
    /// whose time it must be (`state::Stored`): one that fails it is a corrupt store, never
    /// no mark.
    async fn missed_marks(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        self.local_scan::<u64>(prefix).await
    }

    /// The marks of `missed` items (`mi/`), each read bound to its key (`state::Stored`),
    /// with its full key: one that fails it is a corrupt store, never no mark.
    async fn missed_item_marks(&self) -> Result<Vec<(String, MissedItemMark)>> {
        Ok(self
            .local_scan::<MissedItemMark>(key::MISSED_ITEMS)
            .await?
            .into_iter()
            .map(|(rest, mark)| (format!("{}{rest}", key::MISSED_ITEMS), mark))
            .collect())
    }

    /// The counterparties a `missed` item asks to send again (Paykit spec 9): a payee's
    /// cancellation, or any reply, may be among the entries the service deleted. Each
    /// counterparty of an open request, on either side, and of an unfinished payment, from
    /// whatever records the copy holds, so a request another own app recorded counts too:
    /// - as the payer, each payee of a request not paid, cancelled, rejected or expired;
    /// - as the payee, each payer of a request not rejected, cancelled or receipted, and of
    ///   a proof without a request not receipted yet;
    /// - each payee of a payment in the journal not finished or abandoned;
    /// - each counterparty of an Allowance not ended, rejected or conflicted, whose end may
    ///   be among the deleted entries (Paykit spec 8.3).
    ///
    /// Called once the copy is listed: the copy it reads must not lack records.
    pub(crate) async fn missed_counterparties(
        &self,
    ) -> Result<std::collections::BTreeSet<[u8; 32]>> {
        use crate::journal::Stage;
        use crate::requests::{PayeeStatus, RequestStatus};
        let mut counterparties = std::collections::BTreeSet::new();
        for view in self.requests().await? {
            if !matches!(
                view.status,
                RequestStatus::Paid
                    | RequestStatus::Cancelled
                    | RequestStatus::Rejected
                    | RequestStatus::Expired
            ) {
                counterparties.insert(view.request.payee);
            }
        }
        for activity in self.payee_activity().await? {
            if !matches!(
                activity.status,
                PayeeStatus::Rejected | PayeeStatus::Cancelled | PayeeStatus::Receipted
            ) {
                counterparties.insert(activity.payer);
            }
        }
        for payment in self.payments().await? {
            if !matches!(payment.stage, Stage::Finished | Stage::Abandoned) {
                counterparties.insert(payment.payee);
            }
        }
        counterparties.extend(self.open_allowance_counterparties().await?);
        Ok(counterparties)
    }

    /// Answer a `resend_request` from `requester`, whose app presented `cert` (Paykit spec 9,
    /// Paykit wire 4.12). Returns how many messages were re-sent.
    pub(crate) async fn answer_resend(
        &mut self,
        requester: &[u8; 32],
        cert: &SignedDoc,
        request: &ResendRequest,
    ) -> Result<u64> {
        let Ok(cert) = verify_certificate(cert, requester) else {
            return Ok(0);
        };
        let now = self.now();
        let since = now.saturating_sub(SENT_RETENTION_MS);
        let day = now / DAY_MS;
        let mut count: u64 = self
            .local_get(&key::resent_day(requester, day))
            .await?
            .unwrap_or(0);
        let mut resent = 0u64;
        // A time selector matches a message's first acceptance or any later re-send's
        // (Paykit wire 4.12). The client keeps every acceptance time and selects by any of
        // them in a range, but reports only the first with each message: the range is read
        // once, and its messages are selected by time; the other selectors are evaluated
        // below without times.
        let by_time: std::collections::BTreeSet<String> = match request.accepted_from {
            Some(from) => self
                .app
                .sent_to(requester, from, request.accepted_until.unwrap_or(u64::MAX))
                .await?
                .into_iter()
                .map(|sent| sent.msg_id)
                .collect(),
            None => std::collections::BTreeSet::new(),
        };
        for sent in self.app.sent_to(requester, since, u64::MAX).await? {
            if count >= self.config.resend_budget {
                break;
            }
            let Ok(message) = Message::parse(sent.plaintext.json(), OPAQUE_MEMBERS) else {
                continue;
            };
            let first = sent.accepted_at_ms.unwrap_or(0);
            // Only messages first accepted in the last 30 days (Paykit spec 9).
            if first < since {
                continue;
            }
            let period = paykit_lib::Body::parse(&message)
                .ok()
                .and_then(|b| b.billing_period().copied());
            let selected = by_time.contains(&sent.msg_id)
                || request.selects(&message, &sent.plaintext.hash(), &[], period.as_ref());
            if !selected {
                continue;
            }
            if !self.requester_takes(&message, cert.scopes()).await? {
                continue;
            }
            let last: Option<u64> = self
                .local_get(&key::resent(requester, &sent.msg_id))
                .await?;
            // The cooldown is judged, and stamped, on the clock now: the loop awaits the store
            // and the network between messages.
            let now = self.now();
            // A time ahead of the clock (restored, or the clock set back) is due: it never
            // holds a re-send back for longer than the hour.
            if last.is_some_and(|at| at <= now && now - at < RESEND_COOLDOWN_MS) {
                continue;
            }
            // The limits are counted before the re-send goes out (Paykit spec 9): a crash
            // after it can't let the message out again within the hour, or past the day's
            // budget. A re-send that then fails counts all the same.
            count = count.saturating_add(1);
            let mut tx = Tx::default();
            tx.put(&key::resent(requester, &sent.msg_id), &now)?;
            tx.put(&key::resent_day(requester, day), &count)?;
            self.local_commit(tx).await?;
            if self.app.resend(&sent.msg_id).await.is_ok() {
                resent = resent.saturating_add(1);
            }
        }
        Ok(resent)
    }

    /// Whether the requesting app takes `message` (Paykit wire 4.12): its kind is one its
    /// certificate receives, and a payer's reply only under the label rule of Paykit spec 3.4.
    async fn requester_takes(&self, message: &Message, scopes: &[String]) -> Result<bool> {
        let Some(kind) = Kind::from_kind(&message.kind) else {
            return Ok(false);
        };
        if !holds_role(scopes, kind.recipients()) {
            return Ok(false);
        }
        if !kind.is_payer_reply() {
            return Ok(true);
        }
        let side = if kind == Kind::PaymentRequestCancellation {
            // The side is kept with every cancellation this app sends. One without it isn't
            // re-sent: taking either side would guess which apps may get it (Paykit spec
            // 3.4).
            let kept = self
                .local_get::<KeptSide>(&key::side(&message.msg_id))
                .await?;
            // Bound to its key: a side kept for another message doesn't read.
            match kept.map(|kept| kept.side) {
                Some(SideTag::Payee) => Side::Payee,
                Some(SideTag::Payer) => Side::Payer,
                None => return Ok(false),
            }
        } else {
            Side::Payer
        };
        Ok(targets_app(
            kind,
            side,
            body_label(message).as_ref(),
            scopes,
        ))
    }
}
