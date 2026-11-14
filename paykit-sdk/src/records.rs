//! Reading and writing Paykit's shared records through the client (Paykit spec 6, Paykit
//! wire 5): every record is bound before it is acted on, its key recomputed from what its
//! messages name through the client, which blinds with the identity's naming key (Switch
//! wire 1.5), and every write is the client's compare-and-swap.
//!
//! A message whose sending changes a record is signed first, then the record's next value
//! is built from the signed message, holding its stored copy, and the record write and the
//! message go in one batch ([`Paykit::send_with_record`]): the message is queued only if
//! the write is applied, and the write is never made without the message (Switch spec 8.2).
//!
//! **An own message in a record is one to send.** No path stores a message this app signed
//! in a record without queuing it or owing it: a request and the replies go with
//! [`Paykit::send_with_record`] or the request's own batch, a proof in the outcome's batch
//! that queues it, and an acceptance only once it was queued with its payment
//! (`acceptance_released`). The one exception is a rejection or payer's cancellation to a
//! payee whose sends are held ([`Paykit::record_held`]): its batch queues nothing and writes
//! the held answer that names it (`ha/`), from which [`Paykit::queue_own_again`] queues it
//! once sends to the payee aren't held. So a message in a record signed by this app means
//! one to be sent, which the re-send after a restore reads it as;
//! `test_records_own_message_written_only_where_it_is_queued` guards it.

use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::kinds::Kind;
use paykit_lib::records::{ActivityRecord, ActivitySide, PaymentListRecord, StoredMessage};
use paykit_lib::Label;
use pubky_switch_client::app::{LocalWrite, OpBatch, OutgoingMessage, RecordTarget, SignedMessage};
use pubky_switch_core::enc::{serde_b64, serde_b64_vec};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::message::MessagePlaintext;
use pubky_switch_core::ops::{Condition, OpKind};
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::activity::{self, ActivityTarget, Role};
use crate::error::{PaykitError, Result};
use crate::paykit::Paykit;
use crate::receive::Settlement;

/// A verified message kept in Paykit's local state, to be stored in a record later: its
/// sender, the certificate it was received with, its signed plaintext and its acceptance
/// time. It is verified again when it is taken back ([`KeptMessage::stored`]).
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct KeptMessage {
    #[serde(with = "serde_b64")]
    sender: [u8; 32],
    cert: SignedDoc,
    #[serde(with = "serde_b64_vec")]
    plaintext: Vec<u8>,
    accepted_at: Option<u64>,
    /// The feed `seq` its entry was read at, for the members of the bounded pools, the kept
    /// replies (`pr/`, every kind) and the kept stops (`kc/` and `ae/`, counted together),
    /// for an Allowance's conflict stop (`cf/`), ordered among its record's answers, and for
    /// the messages of the stored entries (`se/`) and of the writes kept over a divergent
    /// record (`dw/`): the inbox's position for the entry, which orders it within one
    /// millisecond ([`KeptMessage::order`]). The receive step takes it from the entry it is
    /// handed, a stored entry's note from the entry the client stored, which keeps the `seq`
    /// it was read at; so every one of them is ordered by the same token, whichever store
    /// keeps it. The binding refuses a member of those families without it, and one of any
    /// other family with it (`state::Stored`): a held list (`lh/`) is ordered by nothing and
    /// carries none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seq: Option<u64>,
}

/// The order the inbox accepted a kept message in (Paykit spec 8.1): by acceptance time,
/// then, within one millisecond, by the feed `seq` its entry was read at, the position the
/// inbox gives each entry as it accepts it, whoever sent it (Switch spec 8.3), never by its
/// `msg_id`, which its sender chooses, nor by the store that keeps it. A stored entry keeps
/// the `seq` it was read at when the client tries it again, so a message keeps its position
/// whichever store keeps it and however often it is taken. The `seq` starts again at a
/// provider change, and the entry's generation is not handed to the receive step: two
/// messages of one millisecond accepted under two generations are ordered by their `seq`s.
pub(crate) type KeptOrder = (u64, u64);

impl std::fmt::Debug for KeptMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeptMessage(..)")
    }
}

impl KeptMessage {
    /// Keep `stored`.
    pub(crate) fn of(stored: &StoredMessage) -> Self {
        Self {
            sender: stored.sender(),
            cert: stored.cert().signed().clone(),
            plaintext: stored.plaintext().encode(),
            accepted_at: stored.accepted_at(),
            seq: None,
        }
    }

    /// Keep `stored`, read from the feed at `seq` ([`KeptOrder`]).
    pub(crate) fn taken(stored: &StoredMessage, seq: u64) -> Self {
        Self {
            seq: Some(seq),
            ..Self::of(stored)
        }
    }

    /// The feed `seq` its entry was read at, for a message ordered among others
    /// ([`KeptOrder`]).
    pub(crate) fn seq(&self) -> Option<u64> {
        self.seq
    }

    /// The order its inbox accepted it in ([`KeptOrder`]): one without its acceptance time
    /// or its `seq` is a corrupt store.
    pub(crate) fn order(&self) -> Result<KeptOrder> {
        match (self.accepted_at, self.seq) {
            (Some(at), Some(seq)) => Ok((at, seq)),
            _ => Err(PaykitError::corrupt()),
        }
    }

    /// Whether the kept plaintext reads, without the recipient a full check needs.
    pub(crate) fn check_shape(&self) -> Result<()> {
        MessagePlaintext::decode(&self.plaintext)
            .map(|_| ())
            .map_err(|_| PaykitError::corrupt())
    }

    /// Bound the acceptance time kept to `bound`: whether it was later.
    pub(crate) fn bound_acceptance(&mut self, bound: u64) -> bool {
        match self.accepted_at {
            Some(at) if at > bound => {
                self.accepted_at = Some(bound);
                true
            }
            _ => false,
        }
    }

    /// The stored message again, verified for `recipient` as a record's copy is (Paykit wire
    /// 5.2). A kept message that no longer reads is a corrupt store.
    pub(crate) fn stored(&self, recipient: &[u8; 32]) -> Result<StoredMessage> {
        let plaintext =
            MessagePlaintext::decode(&self.plaintext).map_err(|_| PaykitError::corrupt())?;
        StoredMessage::new(
            self.sender,
            &self.cert,
            plaintext,
            self.accepted_at,
            recipient,
        )
        .map_err(|_| PaykitError::corrupt())
    }
}

impl Paykit {
    /// The record key of an activity target.
    pub(crate) fn activity_key(&self, target: &ActivityTarget) -> Result<[u8; 32]> {
        Ok(self.app.record_key(&target.record_target())?)
    }

    /// The activity record the app holds at `target`, bound to its key (Paykit wire 5.3);
    /// `None` when it holds none, or one that doesn't bind.
    pub(crate) async fn held_activity(
        &self,
        target: &ActivityTarget,
    ) -> Result<Option<ActivityRecord>> {
        let key = self.activity_key(target)?;
        let Some(held) = self.app.shared_record(&key).await? else {
            return Ok(None);
        };
        let Some(value) = held.content.value() else {
            return Ok(None);
        };
        Ok(bind_activity(
            value,
            &self.identity(),
            target.side.side(),
            target.label().as_ref(),
            &key,
            |t| self.app.record_key(t).ok(),
            &|p| self.rail.settlement_id(p),
        ))
    }

    /// The activity record at `target`, read from the switch when the app holds none.
    pub(crate) async fn activity(
        &mut self,
        target: &ActivityTarget,
    ) -> Result<Option<ActivityRecord>> {
        if let Some(record) = self.held_activity(target).await? {
            return Ok(Some(record));
        }
        self.app.read_record(&target.record_target()).await?;
        self.held_activity(target).await
    }

    /// The write of the record at `target` with `stored` placed in its `role` (Paykit spec 6
    /// "One message per role"), built on the version the app holds, with `conditions`; a
    /// record that doesn't read is written again with the message (Paykit wire 5.3).
    /// `None` when the record already reflects the message.
    pub(crate) async fn placed_write(
        &mut self,
        target: &ActivityTarget,
        counterparty: [u8; 32],
        role: Role,
        stored: StoredMessage,
        conditions: Vec<Condition>,
    ) -> Result<Option<(Vec<Condition>, OpKind)>> {
        self.placed_write_with(target, counterparty, role, stored, conditions, false, false)
            .await
    }

    /// [`Paykit::placed_write`], setting `manual_only` (Paykit wire 5.3) on a record it opens
    /// when `manual_only`: a one-time request that came back after the user's block
    /// (`receive.rs`); and, when `reflect_only`, the write that reflects `stored` by its
    /// `msg_id` alone, leaving every role as it is ([`activity::reflect`]).
    #[allow(clippy::too_many_arguments)] // the write's target and message, and its two flags
    pub(crate) async fn placed_write_with(
        &mut self,
        target: &ActivityTarget,
        counterparty: [u8; 32],
        role: Role,
        stored: StoredMessage,
        conditions: Vec<Condition>,
        manual_only: bool,
        reflect_only: bool,
    ) -> Result<Option<(Vec<Condition>, OpKind)>> {
        let own = self.identity();
        let side = target.side.side();
        let key = self.activity_key(target)?;
        let held = self.app.shared_record(&key).await?;
        // Every own write takes the record's view first: one written over a divergent view,
        // a stop included, leaves the record paused (`divergent.rs`).
        if let Some(seen) = &held {
            Box::pin(self.pause_of(seen)).await?;
        }
        let current = held.as_ref().and_then(|r| r.content.value());
        let held_record = activity::read(current, &own, side);
        let opens = held_record.is_none();
        let mut record = held_record.unwrap_or_else(|| activity::empty(side, counterparty));
        let placed = if reflect_only {
            activity::reflect(&mut record, &stored)
        } else {
            activity::place(&mut record, role, stored)
        };
        if !placed {
            return Ok(None);
        }
        if manual_only && opens {
            record.manual_only = true;
        }
        let value = record.to_json()?;
        Ok(Some(
            self.app
                .record_op(&target.record_target(), Some(&value), conditions)
                .await?,
        ))
    }

    /// Every `payment_lists` record of `payee` the app holds that binds.
    pub(crate) async fn lists_of(&self, payee: &[u8; 32]) -> Result<Vec<PaymentListRecord>> {
        let own = self.identity();
        let mut out = Vec::new();
        for record in self.app.records_of(Collection::PaymentLists.name()).await? {
            let Some(value) = record.content.value() else {
                continue;
            };
            if let Some(list) = bind_list(value, &own, &record.key, |t| self.app.record_key(t).ok())
            {
                if &list.payee == payee {
                    out.push(list);
                }
            }
        }
        Ok(out)
    }

    /// Sign `body` once as a message of `kind` to `to`, keeping nothing and sending nothing
    /// (Switch spec 8.2): its hash is known before anything that names it is written.
    pub(crate) fn sign(
        &self,
        to: [u8; 32],
        kind: Kind,
        refs: Vec<[u8; 32]>,
        body: Map<String, Value>,
    ) -> Result<SignedMessage> {
        let mut message = OutgoingMessage::new(to, kind.as_str(), body);
        message.refs = refs;
        Ok(self.app.sign_message(message)?)
    }

    /// The stored copy of a message this app signed, as its own records hold it: its
    /// acceptance time isn't known yet (Paykit wire 5.2).
    pub(crate) fn own_copy(&self, signed: &SignedMessage) -> Result<StoredMessage> {
        Ok(StoredMessage::new(
            self.identity(),
            self.app.cert().signed(),
            signed.plaintext().clone(),
            None,
            &signed.to(),
        )?)
    }

    /// Place `signed` in its `role` of the record at `target` and queue it, in one batch with
    /// `conditions` on the record write and the local writes `local` (Switch spec 8.2): the
    /// message goes out only if the record write is applied. Returns the `msg_id`. Nothing is
    /// sent: the caller flushes. The way an own message goes into a record: never stored
    /// without being queued, so one in a record is one to be sent (module docs).
    ///
    /// # Errors
    ///
    /// The client's errors. After `409 conflict` the record is read again first, so the
    /// caller decides again on what the switch holds; nothing was queued.
    pub(crate) async fn send_with_record(
        &mut self,
        signed: SignedMessage,
        target: &ActivityTarget,
        role: Role,
        conditions: Vec<Condition>,
        local: Vec<LocalWrite>,
    ) -> Result<String> {
        let msg_id = signed.msg_id().to_owned();
        let stored = self.own_copy(&signed)?;
        let write = self
            .placed_write(target, signed.to(), role, stored, conditions)
            .await?;
        let mut batch = OpBatch::new().queue(signed.clone()).locals(local.clone());
        match write {
            Some(entry) => batch = batch.entry(entry),
            // The record reflects it already: the message is queued alone, with the local
            // writes.
            None => {
                self.app.queue_signed(signed).await?;
                if !local.is_empty() {
                    self.app.protocol_commit(&local).await?;
                }
                return Ok(msg_id);
            }
        }
        match self.app.submit_batch(batch).await {
            Ok(_) => Ok(msg_id),
            Err(error) => {
                if error.code() == Some(ErrorCode::Conflict) {
                    self.app.read_record(&target.record_target()).await?;
                }
                Err(error.into())
            }
        }
    }

    /// Place `signed`, a rejection or payer's cancellation to a payee whose sends are held,
    /// in its `role` of the record at `target` WITHOUT queuing it, in one batch with
    /// `conditions` on the record write, the local writes `local`, and the held answer that
    /// names it ([`crate::hold::HeldAnswer`]) for `request_hash` (Paykit spec 8.1, "Blocked
    /// counterparties"). The record then stops every own app from paying or accepting the
    /// request, and the message goes from the record once sends to the payee aren't held
    /// ([`Paykit::queue_own_again`]): an own message in a record is still one to be sent
    /// (module docs). Returns the `msg_id`. Nothing is queued, so no flush sends it.
    ///
    /// # Errors
    ///
    /// As [`Paykit::send_with_record`].
    pub(crate) async fn record_held(
        &mut self,
        signed: SignedMessage,
        target: &ActivityTarget,
        role: Role,
        conditions: Vec<Condition>,
        mut local: Vec<LocalWrite>,
        request_hash: [u8; 32],
    ) -> Result<String> {
        let msg_id = signed.msg_id().to_owned();
        let payee = signed.to();
        local.push(crate::state::local_put(
            &crate::state::key::held_answer(&payee, &msg_id),
            &crate::hold::HeldAnswer {
                payee,
                msg_id: msg_id.clone(),
                request_hash,
            },
        )?);
        let stored = self.own_copy(&signed)?;
        let write = self
            .placed_write(target, payee, role, stored, conditions)
            .await?;
        let Some(entry) = write else {
            // The record reflects it already: the held answer alone, with the local writes.
            self.app.protocol_commit(&local).await?;
            return Ok(msg_id);
        };
        match self
            .app
            .submit_batch(OpBatch::new().locals(local).entry(entry))
            .await
        {
            Ok(_) => Ok(msg_id),
            Err(error) => {
                if error.code() == Some(ErrorCode::Conflict) {
                    self.app.read_record(&target.record_target()).await?;
                }
                Err(error.into())
            }
        }
    }
}

/// Bind an activity record value held at `key` under the policy of `label` (Paykit wire
/// 5.1, 5.3, Switch spec 8.2): it reads as `own` holds it, its labels are its policy's, and
/// `key` is the one its messages name, as `record_key` derives it, a payee's settlement read
/// by `settlement`, the app's rail. `None`: the reader doesn't act on it.
pub(crate) fn bind_activity(
    value: &[u8],
    own: &[u8; 32],
    side: ActivitySide,
    label: Option<&Label>,
    key: &[u8; 32],
    record_key: impl Fn(&RecordTarget) -> Option<[u8; 32]>,
    settlement: Settlement<'_>,
) -> Option<ActivityRecord> {
    let record = ActivityRecord::read(value, own, side).ok()?;
    record.check_label(label).ok()?;
    let natural = record.natural_key_with(own, settlement).ok()?;
    let target = RecordTarget::new(
        activity::switch_collection(side.collection(), label),
        &natural,
    );
    (record_key(&target)? == *key).then_some(record)
}

/// Bind a `payment_lists` value held at `key` (Paykit wire 5.4): `key` is the one derived
/// from its `payee` and `app_id`.
pub(crate) fn bind_list(
    value: &[u8],
    own: &[u8; 32],
    key: &[u8; 32],
    record_key: impl Fn(&RecordTarget) -> Option<[u8; 32]>,
) -> Option<PaymentListRecord> {
    let record = PaymentListRecord::read(value, own).ok()?;
    (record_key(&list_target(&record.payee, &record.app_id))? == *key).then_some(record)
}

/// The client's target of the `payment_lists` record of a payee app.
pub(crate) fn list_target(payee: &[u8; 32], app_id: &[u8; 16]) -> RecordTarget {
    RecordTarget::new(
        activity::switch_collection(Collection::PaymentLists, None),
        &natural_key::payment_list(payee, app_id),
    )
}

/// A `409 conflict`: the caller reads again and decides again.
pub(crate) fn conflict(error: PaykitError) -> PaykitError {
    if error.code() == Some(ErrorCode::Conflict) {
        PaykitError::Conflict("the record changed meanwhile".into())
    } else {
        error
    }
}

#[cfg(test)]
mod own_message_tests {
    /// Every Rust source file of the crate, by its path under `src/`, with its text.
    fn sources() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let name = path
                        .strip_prefix(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
                        .unwrap()
                        .display()
                        .to_string();
                    let mut text = std::fs::read_to_string(&path).unwrap();
                    // This test's own text names what it counts.
                    if let Some(at) = text.find("mod own_message_tests") {
                        text.truncate(at);
                    }
                    out.push((name, text));
                }
            }
        }
        let mut out = Vec::new();
        walk(
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")),
            &mut out,
        );
        out.sort();
        out
    }

    /// The text of the function `name` in `text`: from its signature to the first line
    /// closing it at its own indentation.
    fn body<'a>(text: &'a str, name: &str) -> &'a str {
        let at = text
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("no fn {name}"));
        let start = text[..at].rfind('\n').map_or(0, |n| n + 1);
        let indent = &text[start..at]
            .chars()
            .take_while(|c| *c == ' ')
            .collect::<String>();
        let close = format!("\n{indent}}}\n");
        let end = text[at..].find(&close).map_or(text.len(), |n| at + n);
        &text[at..end]
    }

    #[test]
    fn test_records_own_message_written_only_where_it_is_queued() {
        // The stored copy of a message this app signed is made in four places, each in the
        // batch that queues it or owes it: `send_with_record`, `record_held`, which writes
        // the held answer that names it, the request's own batch, and the outcome's batch
        // with its proof; and an acceptance is placed only once released. A new place that
        // stores an own message must queue or owe it in the same batch, so that an own
        // message in a record is always one to be sent (module docs).
        let sources = sources();
        let text = |name: &str| -> &str {
            &sources
                .iter()
                .find(|(file, _)| file == name)
                .unwrap_or_else(|| panic!("no {name}"))
                .1
        };
        let count = |needle: &str| -> Vec<(String, usize)> {
            sources
                .iter()
                .map(|(name, text)| {
                    let n = text
                        .lines()
                        .filter(|line| !line.trim_start().starts_with("//"))
                        .filter(|line| line.contains(needle))
                        .count();
                    (name.clone(), n)
                })
                .filter(|(_, n)| *n > 0)
                .collect()
        };
        let named = |pairs: &[(&str, usize)]| -> Vec<(String, usize)> {
            pairs.iter().map(|(n, c)| ((*n).to_owned(), *c)).collect()
        };
        assert_eq!(
            count("self.own_copy("),
            named(&[
                ("allowances.rs", 3),
                ("pay.rs", 1),
                ("records.rs", 2),
                ("requests.rs", 1),
            ]),
            "an own message's stored copy is made somewhere new: queue it in the same batch"
        );
        // Every stored message is built in one of these: a received message kept
        // (`KeptMessage::stored`) or stored by the client (`note_stored`), an own message's
        // copy (`own_copy`), an acceptance journaled (`acceptance_message`), and the unit
        // tests' parties and copies (`as_read`). A copy built anywhere else could store an own message unqueued.
        assert_eq!(
            count("StoredMessage::new("),
            named(&[
                ("allowances.rs", 1),
                ("journal.rs", 1),
                ("records.rs", 2),
                ("stored.rs", 1),
                ("testkit.rs", 1),
            ]),
            "a stored message is built somewhere new"
        );
        assert!(body(text("records.rs"), "own_copy").contains("StoredMessage::new("));
        assert!(body(text("allowances.rs"), "as_read").contains("StoredMessage::new("));
        assert!(body(text("journal.rs"), "acceptance_message").contains("StoredMessage::new("));
        assert!(body(text("stored.rs"), "note_stored").contains("StoredMessage::new("));
        // Each place that makes an own copy queues it with the record write.
        let send = body(text("records.rs"), "send_with_record");
        assert!(send.contains("self.own_copy(&signed)"));
        assert!(send.contains(".queue(signed.clone())"));
        assert!(send.contains("self.app.queue_signed(signed)"));
        // The held path queues nothing and owes the message in the same batch.
        let held = body(text("records.rs"), "record_held");
        assert!(held.contains("self.own_copy(&signed)"));
        assert!(held.contains("key::held_answer(&payee, &msg_id)"));
        assert!(held.contains("OpBatch::new().locals(local).entry(entry)"));
        assert!(!held.contains(".queue("));
        assert!(!held.contains("queue_signed"));
        let request = body(text("requests.rs"), "send_request_inner");
        assert!(request.contains("self.own_copy(&signed)"));
        assert!(request.contains(".queue(signed)"));
        // An Allowance's proposal, answer or end, and an end crossing a withdrawal, queue
        // the message in the batch that writes the record (`allowances.rs`, whose own test
        // checks every record write goes through these).
        let allowances = text("allowances.rs");
        let propose = body(allowances, "propose_allowance_inner");
        assert!(propose.contains("self.own_copy(&signed)"));
        assert!(propose.contains(".queue(signed)"));
        let answer = body(allowances, "answer_allowance");
        assert!(answer.contains("self.own_copy(&signed)"));
        assert!(answer.contains("let mut queue = vec![signed];"));
        assert!(answer.contains(".submit_with_record(&target, &next, queue, locals, None)"));
        let crossing = body(allowances, "record_end");
        assert!(crossing.contains("self.own_copy(&own_end)"));
        assert!(crossing.contains("queue.push(own_end);"));
        assert!(
            crossing.contains(".submit_with_record(target, &next, queue, locals, owed.as_ref())")
        );
        // While sends are held the crossing end is recorded unqueued, with its owed mark
        // (`eo/`) written in the same batch by `submit_with_record`, and sent from the record
        // later (`send_owed_ends`).
        assert!(crossing.contains("owed = Some(OwedEnd {"));
        let submit = body(allowances, "submit_with_record");
        assert!(
            submit.contains("locals.push(state::local_put(&key::owed_end(&owed.record), owed)?);")
        );
        // The journal's own check reads it; only the outcome places it.
        assert_eq!(
            count(".acceptance_message("),
            named(&[("journal.rs", 1), ("pay.rs", 1)]),
            "an acceptance is placed somewhere new"
        );
        let pay = text("pay.rs");
        assert!(body(pay, "outcome_record").contains("self.own_copy(proof)"));
        let outcome = body(pay, "write_outcome");
        assert!(outcome.contains(".outcome_record(entry, &target, paid, proof.as_ref()"));
        assert!(outcome.contains("batch = batch.queue(proof);"));
        assert!(outcome.contains("self.app.queue_signed(proof)"));
        assert!(body(pay, "place_acceptance").contains("if !entry.acceptance_released {"));
    }
}
