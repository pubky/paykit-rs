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
use pubky_switch_client::app::SENT_RETENTION_MS;
use pubky_switch_core::enc::unb64_array;
use pubky_switch_core::message::Message;
use pubky_switch_core::records::DAY_MS;
use pubky_switch_core::sig::SignedDoc;

use crate::activity::{KeptSide, SideTag};
use crate::error::{PaykitError, Result};
use crate::paykit::Paykit;
use crate::protocol::{body_label, holds_role, MissedItemMark, QueuedAsk};
use crate::state::{key, Tx};

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
    /// the counterparty can't be made to answer. Returns the request's `msg_id`.
    ///
    /// # Errors
    ///
    /// [`paykit_lib::Error::Validation`] for a selection that selects nothing; the client's
    /// errors.
    pub async fn request_resend(
        &mut self,
        counterparty: [u8; 32],
        selection: ResendSelection,
    ) -> Result<String> {
        self.refresh_own()?;
        let signed = self.resend_request_message(counterparty, selection)?;
        let msg_id = self.app.queue_signed(signed).await?;
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
    /// "Blocked counterparties"). Returns how many requests went out.
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
        for (rest, ask) in asks {
            if self.sends_held(&ask.sender).await? {
                continue;
            }
            self.request_resend(
                ask.sender,
                ResendSelection {
                    accepted_from_ms: Some(ask.from_ms),
                    accepted_until_ms: ask.until_ms,
                    ..ResendSelection::default()
                },
            )
            .await?;
            self.local_delete(&format!("{}{rest}", key::ASKS)).await?;
            sent = sent.saturating_add(1);
        }
        Ok(sent)
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
    /// - each payee of a payment in the journal not finished or abandoned.
    ///
    /// Called once the copy is listed: the copy it reads must not lack records. Allowances
    /// add theirs here.
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
