//! Extension kinds (Paykit spec 3.3, Paykit wire 4.3): message kinds `<prefix>.<name>` under
//! an extension scope `paykit_ext:<prefix>`, which carry no Paykit payment semantics.
//!
//! - **Registering.** The host names the kinds its extension handles
//!   ([`crate::PaykitConfig::with_extension_kinds`]). The app's record accepts each one
//!   whose prefix the certificate holds ([`crate::PaykitProtocol`]'s `accepts`), so senders
//!   wrap them to this app; a kind not registered is dropped at check 10, kept as processed
//!   (Switch wire 5.1).
//! - **Receiving.** A registered kind passes check 10 as its body arrived: the SDK never
//!   parses it beyond the common checks, never records it in a collection, and never takes
//!   it as a destination or an answer. The receive step keeps it for the host
//!   (`xm/<sender>/<msg_id>`), in the commit that moves the cursor past it, so a crash
//!   between the read and the host's taking it loses nothing. Reading or restoring a kept
//!   message verifies its sender certificate and signed plaintext for this recipient.
//!   The host reads it with
//!   [`crate::Paykit::extension_messages`] and drops it with
//!   [`crate::Paykit::acknowledge_extension_message`]. It gives its sender no more than a
//!   Paykit message would: at an app holding `paykit_contacts`, one from a sender the deny list
//!   holds, or the client flags, is counted by the spam rule and not kept, and while the
//!   copy of the deny list isn't known the entry is held, never dropped (Paykit wire 4.1).
//!   At most [`MAX_EXTENSION_MESSAGES_PER_SENDER`] per sender and
//!   [`MAX_EXTENSION_MESSAGES`] in all are kept untaken. Past them one kept goes as one never
//!   processed, as a kept reply does: past the sender's own bound its message accepted
//!   earliest, else the earliest of the sender holding the most, the lower identity on a tie.
//!   Selection includes the input. Eviction records its recovery mark atomically. A
//!   protected earlier victim leaves the incoming message waiting for admission.
//! - **Sending.** [`crate::Paykit::send_extension`] signs and queues one for a counterparty,
//!   only under a certificate holding `paykit_contacts` and its `paykit_ext:<prefix>`, and refuses, signing and queuing
//!   nothing, while sends to that counterparty are held ([`crate::Paykit::sends_held`]: the
//!   deny list holds it, or its copy isn't known; Paykit spec 8.1).

use paykit_lib::kinds::ExtensionKind;
use pubky_switch_client::app::{
    AppError, Effect, FeedPosition, LocalWrite, Outcome, OutgoingMessage, ReceiveContext,
    ReceivedMessage, Refusal as ClientRefusal,
};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::message::Message;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Refusal, Result};
use crate::paykit::Paykit;
use crate::receive::Fail;
use crate::records::KeptMessage;
use crate::state::{self, key};

/// The most extension messages kept untaken from one sender.
pub const MAX_EXTENSION_MESSAGES_PER_SENDER: usize = 64;

/// The most extension messages kept untaken in all.
pub const MAX_EXTENSION_MESSAGES: usize = 128;

/// An extension message the host hasn't taken yet: its sender, kind, common fields and body
/// as they arrived (Paykit wire 4.3). Its `Debug` shows no body.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ExtensionMessage {
    /// The sender identity.
    #[serde(with = "serde_b64")]
    pub sender: [u8; 32],
    /// The sender app's App ID.
    #[serde(with = "serde_b64")]
    pub sender_app: [u8; 16],
    /// The kind, `<prefix>.<name>`.
    pub kind: String,
    /// The message ID.
    pub msg_id: String,
    /// `created_at`, Unix milliseconds; informational (Switch spec 5.3).
    pub created_at_ms: u64,
    /// When the inbox accepted it, service time in Unix milliseconds.
    pub accepted_at_ms: u64,
    /// The generation of the verified inbox entry.
    pub generation: u64,
    /// The sequence number of the verified inbox entry within its generation.
    pub seq: u64,
    /// The hashes of the messages it refers to, as the extension orders them.
    pub refs: Vec<String>,
    /// The body.
    pub body: Map<String, Value>,
}

impl std::fmt::Debug for ExtensionMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ExtensionMessage({}, {}, ..)", self.kind, self.msg_id)
    }
}

#[derive(Serialize)]
struct ExtensionBody<'a> {
    kind: &'a str,
    body: &'a Map<String, Value>,
}

impl ExtensionBody<'_> {
    fn check(&self) -> std::result::Result<(), pubky_switch_core::Error> {
        let bytes = serde_json::to_vec(self)?;
        pubky_switch_core::json::check_json_as(
            &bytes,
            pubky_switch_core::json::Doc::Message(paykit_lib::kinds::OPAQUE_MEMBERS),
        )
    }
}

/// The signed carrier and its original inbox generation (Switch spec 8.2).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeptExtension {
    message: KeptMessage,
    generation: u64,
}

impl KeptExtension {
    pub(crate) fn open(&self, recipient: &[u8; 32]) -> Result<ExtensionMessage> {
        let message = self.message.extension_for(recipient)?;
        let (accepted_at_ms, seq) = self.message.order()?;
        Ok(ExtensionMessage::of(
            message,
            accepted_at_ms,
            self.generation,
            seq,
        ))
    }
}

impl ExtensionMessage {
    fn position(&self) -> FeedPosition {
        FeedPosition {
            generation: self.generation,
            seq: self.seq,
        }
    }

    fn of(message: Message, accepted_at_ms: u64, generation: u64, seq: u64) -> Self {
        Self {
            sender: message.from.identity,
            sender_app: message.from.app_id,
            kind: message.kind,
            msg_id: message.msg_id,
            created_at_ms: message.created_at_ms,
            accepted_at_ms,
            generation,
            seq,
            refs: message
                .refs
                .iter()
                .map(|r| pubky_switch_core::enc::b64(r))
                .collect(),
            body: message.body,
        }
    }
}

/// The receive step's effect for an extension kind the host registered (module docs): kept
/// for the host, in the step's commit, when its sender may create state and the bounds have
/// room; held while the copy of the deny list isn't known or the bounds are full.
pub(crate) fn extension_effect(
    cx: &mut ReceiveContext<'_>,
    message: &ReceivedMessage,
) -> std::result::Result<Outcome, Fail> {
    let scopes = cx.cert().scopes().to_vec();
    crate::receive::note_cursor_time_at(cx, message.accepted_at_ms)?;
    let incoming = ExtensionMessage::of(
        message.message.clone(),
        message.accepted_at_ms,
        message.generation,
        message.seq,
    );
    let kept_key = key::extension(&incoming.sender, &incoming.msg_id);
    if state::cx_get::<ExtensionMessage>(cx, &kept_key)?.is_some()
        || !crate::receive::admits_new_state_of(
            cx,
            &incoming.sender,
            &scopes,
            &incoming.msg_id,
            incoming.accepted_at_ms,
        )?
    {
        return Ok(Outcome::Apply(Effect::default()));
    }
    let pool: Vec<_> = state::cx_scan::<ExtensionMessage>(cx, key::EXTENSIONS)?
        .into_iter()
        .map(|(_, m)| m)
        .collect();
    let victim = match to_drop(&pool, &incoming, |a, b| cx.received_order(a, b)) {
        Ok(victim) => victim,
        Err(UnknownOrder) => return Ok(Outcome::AwaitAdmission),
    };
    if let Some(victim) = victim {
        let at = victim.accepted_at_ms;
        if victim.sender == incoming.sender && victim.msg_id == incoming.msg_id {
            crate::receive::mark_dropped(cx, [(victim.sender, (at, at))])?;
            return Ok(Outcome::Consume);
        }
        match cx.forget_processed(victim.sender, &victim.msg_id) {
            Ok(_) => {}
            Err(AppError::Refused(ClientRefusal::NotNow(_))) => return Ok(Outcome::AwaitAdmission),
            Err(error) => return Err(error.into()),
        }
        crate::receive::mark_dropped(cx, [(victim.sender, (at, at))])?;
        crate::receive::keep_local(
            cx,
            LocalWrite::Delete {
                key: key::extension(&victim.sender, &victim.msg_id).into_bytes(),
            },
        );
    }
    let kept = KeptExtension {
        message: KeptMessage::received(message),
        generation: message.generation,
    };
    let write = state::local_put(&kept_key, &kept).map_err(|_| Fail::corrupt())?;
    crate::receive::keep_local(cx, write);
    Ok(Outcome::Apply(Effect::default()))
}

#[derive(Debug)]
struct UnknownOrder;

/// Apply both bounds to the pool including the incoming message (Paykit spec 3.3).
fn to_drop<'a>(
    kept: &'a [ExtensionMessage],
    incoming: &'a ExtensionMessage,
    compare: impl Fn(FeedPosition, FeedPosition) -> Option<std::cmp::Ordering>,
) -> std::result::Result<Option<&'a ExtensionMessage>, UnknownOrder> {
    let pool = kept.iter().chain(std::iter::once(incoming));
    let sender = &incoming.sender;
    let mut counts = std::collections::BTreeMap::<[u8; 32], usize>::new();
    for message in pool.clone() {
        *counts.entry(message.sender).or_default() += 1;
    }
    let from = if counts.get(sender).copied().unwrap_or(0) > MAX_EXTENSION_MESSAGES_PER_SENDER {
        sender
    } else if kept.len() >= MAX_EXTENSION_MESSAGES {
        counts
            .iter()
            .max_by(|(a, n), (b, m)| n.cmp(m).then_with(|| b.cmp(a)))
            .map(|(sender, _)| sender)
            .ok_or(UnknownOrder)?
    } else {
        return Ok(None);
    };
    let candidates = pool.filter(|message| message.sender == *from);
    let earliest = candidates
        .clone()
        .map(|message| message.accepted_at_ms)
        .min()
        .ok_or(UnknownOrder)?;
    let tied = candidates.filter(|message| message.accepted_at_ms == earliest);
    for candidate in tied.clone() {
        if tied.clone().all(|other| {
            compare(candidate.position(), other.position()).is_some_and(|order| !order.is_gt())
        }) {
            return Ok(Some(candidate));
        }
    }
    Err(UnknownOrder)
}

impl Paykit {
    /// Bring the record's `accepts` to the kinds the protocol handles for the certificate,
    /// the registered extension kinds included, when they differ (Switch spec 3.4,
    /// [`pubky_switch_client::app::App::update_accepts`]): the record is signed again,
    /// delivered and published. A delivery that fails stays owed by the client, which sends
    /// it again at a later read; only a store failure or an alarm is an error.
    ///
    /// # Errors
    ///
    /// A store failure or an alarm.
    pub(crate) async fn align_accepts(&mut self) -> Result<()> {
        use pubky_switch_client::app::{AppError, Protocol};
        let is_extension = |kind: &String| ExtensionKind::parse(kind).is_ok();
        let mut wanted = self.protocol.accepts(self.app.cert());
        wanted.retain(is_extension);
        let mut held =
            pubky_switch_core::records::AppRecord::verify(self.app.record(), &self.identity())
                .map(|record| record.body().accepts.clone())
                .unwrap_or_default();
        held.retain(is_extension);
        wanted.sort();
        held.sort();
        if wanted == held {
            return Ok(());
        }
        match self.app.update_accepts().await {
            Ok(_) => Ok(()),
            Err(error @ (AppError::Store(_) | AppError::Alarm(_))) => Err(error.into()),
            Err(_) => Ok(()),
        }
    }

    /// The extension messages received and not yet taken, oldest key first (module docs).
    /// Each is kept until [`Paykit::acknowledge_extension_message`].
    ///
    /// # Errors
    ///
    /// A store failure, or a corrupt store.
    pub async fn extension_messages(&self) -> Result<Vec<ExtensionMessage>> {
        Ok(self
            .local_scan::<ExtensionMessage>(key::EXTENSIONS)
            .await?
            .into_iter()
            .map(|(_, message)| message)
            .collect())
    }

    /// Drop the extension message `msg_id` from `sender` once the host has taken it; nothing
    /// for one not kept.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn acknowledge_extension_message(
        &mut self,
        sender: &[u8; 32],
        msg_id: &str,
    ) -> Result<()> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        self.local_delete(&key::extension(sender, msg_id)).await
    }

    /// Send `to` an extension message of `kind`, with `refs` and `body` (Paykit spec 3.3,
    /// Paykit wire 4.3), queued and flushed; returns its `msg_id`. It goes only to the
    /// recipient's apps whose certificate holds `paykit_ext:<prefix>` and whose record accepts the
    /// kind (Switch spec 3.4). The body follows the JSON rules of every message: a decimal
    /// is written as a string, outside `metadata`.
    ///
    /// # Errors
    ///
    /// [`Refusal::CertificateRefused`]; [`Refusal::ScopeMissing`] without `paykit_contacts` or `paykit_ext:<prefix>`;
    /// [`Refusal::Usage`] for the app's own identity, or a body the switch would refuse;
    /// [`Refusal::NotNow`] while sends to `to` are held, the deny list holding it or its
    /// copy not known, with nothing signed or queued; the client's errors.
    pub async fn send_extension(
        &mut self,
        to: [u8; 32],
        kind: &ExtensionKind,
        refs: Vec<[u8; 32]>,
        body: Map<String, Value>,
    ) -> Result<String> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        if !self.holds("paykit_contacts") {
            return Err(Refusal::ScopeMissing("paykit_contacts".into()).into());
        }
        let scope = kind.prefix().scope();
        if !self.app.cert().scopes().contains(&scope) {
            return Err(Refusal::ScopeMissing(scope).into());
        }
        if to == self.identity() {
            return Err(Refusal::Usage(
                "Paykit spec 3.3: an extension message goes to another identity".into(),
            )
            .into());
        }
        self.refuse_while_sends_held(&to).await?;
        let kind = kind.as_string();
        ExtensionBody {
            kind: &kind,
            body: &body,
        }
        .check()
        .map_err(|_| Refusal::Usage("Paykit wire 4.3: invalid extension body".into()))?;
        let mut message = OutgoingMessage::new(to, &kind, body);
        message.refs = refs;
        let signed = self.app.sign_message(message)?;
        let msg_id = self.app.queue_signed(signed).await?;
        // Once queued, the host can track this ID and retry a failed flush.
        let _ = self.send_message(&msg_id).await;
        Ok(msg_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(sender: u8, accepted_at_ms: u64, seq: u64) -> ExtensionMessage {
        ExtensionMessage {
            sender: [sender; 32],
            sender_app: [0; 16],
            kind: "paykit-cap.offer".into(),
            msg_id: format!("{sender:02x}{seq:06x}-0000-4000-8000-000000000000"),
            created_at_ms: 0,
            accepted_at_ms,
            generation: 1,
            seq,
            refs: Vec::new(),
            body: Map::new(),
        }
    }

    fn by_seq(a: FeedPosition, b: FeedPosition) -> Option<std::cmp::Ordering> {
        (a.generation == b.generation).then(|| a.seq.cmp(&b.seq))
    }

    #[test]
    fn test_extensions_drop_past_a_senders_bound_its_earliest() {
        let mut held: Vec<_> = (0..MAX_EXTENSION_MESSAGES_PER_SENDER as u64)
            .map(|i| kept(1, 100 - i, i))
            .collect();
        held.push(kept(2, 1, 99));
        // Sender 1 is at its bound: its message accepted earliest goes, never sender 2's.
        let incoming = kept(1, 1000, 1000);
        let gone = to_drop(&held, &incoming, by_seq).unwrap().unwrap();
        assert_eq!(gone.sender, [1; 32]);
        assert_eq!(
            gone.accepted_at_ms,
            100 - (MAX_EXTENSION_MESSAGES_PER_SENDER as u64 - 1)
        );
        // Below both bounds nothing goes.
        assert!(to_drop(&held, &kept(2, 2, 100), by_seq).unwrap().is_none());
        let earlier = kept(1, 1, 1001);
        assert_eq!(to_drop(&held, &earlier, by_seq).unwrap(), Some(&earlier));
    }

    #[test]
    fn test_extensions_drop_past_the_bound_on_all_from_the_sender_holding_the_most() {
        // 50 of sender 3, 50 of sender 2, 28 of sender 1: past 128 the earliest of sender 2
        // goes, the lower identity among the two holding the most, though sender 1 holds the
        // earliest message of all.
        let mut held = Vec::new();
        for i in 0..50 {
            held.push(kept(3, 10 + i, i));
            held.push(kept(2, 20 + i, 100 + i));
        }
        for i in 0..28 {
            held.push(kept(1, i, 200 + i));
        }
        let incoming = kept(4, 80, 300);
        assert!(to_drop(&held[..127], &incoming, by_seq).unwrap().is_none());
        let gone = to_drop(&held, &incoming, by_seq).unwrap().unwrap();
        assert_eq!(gone.sender, [2; 32]);
        assert_eq!(gone.accepted_at_ms, 20);
        // On equal acceptance times the earlier intake goes.
        let mut tie: Vec<_> = (0..MAX_EXTENSION_MESSAGES_PER_SENDER as u64)
            .map(|i| kept(5, 7, 1000 - i))
            .collect();
        tie.reverse();
        assert_eq!(
            to_drop(&tie, &kept(5, 7, 1001), by_seq)
                .unwrap()
                .unwrap()
                .seq,
            1000 - (MAX_EXTENSION_MESSAGES_PER_SENDER as u64 - 1)
        );
        let mut across_loss = kept(5, 7, 1);
        across_loss.generation = 2;
        assert!(to_drop(&tie, &across_loss, by_seq).is_err());
    }
}
