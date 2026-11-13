//! Marks of a sender's messages dropped before they were taken, and the re-sends they ask
//! for (Paykit spec 8.1, "Replies before their record" and "Stored entries"; Paykit spec 9).
//!
//! Three families, each with one mark per sender identity, the identity its certificate
//! names:
//!
//! - **Replies dropped for the bounds on kept replies** (`pd/<sender>`, `receive.rs`): a
//!   payer's replies at a payee app, and receipts at a payer app. Once the app holds a
//!   record, of either side, whose counterparty holds such a mark (of a request, or of a
//!   payment without a request), it asks that sender to re-send what its inbox accepted
//!   from the mark's earliest time (`resend_request` with `accepted_from`). The client
//!   marked such a reply processed when it was kept, so the step that drops it forgets its
//!   pair, and a re-sent copy is taken as a new message. A payee's cancellation kept as a
//!   stop for a request the app holds no record of, dropped for the bounds on those stops,
//!   counts in the same mark, which is then due at once: the request it cancels may come at
//!   any time.
//! - **Entries dropped unverified** (`ud/<sender>`, `stored.rs`). The app asks that sender
//!   at once to re-send what its inbox accepted from the mark's earliest time to its latest
//!   (`accepted_from` and `accepted_until`).
//! - **Entries dropped while they waited for a scope key** (`kw/<sender>`, `stored.rs`): the
//!   client drops such an entry past its bounds unprocessed (Switch spec 8.2). The mark also
//!   holds the data epochs the messages waited for, and asks that sender, as for entries
//!   dropped unverified, only while the app holds every key of those epochs its certificate
//!   is given: a copy sent before would wait for the same key.
//!
//! **Two times.** A mark holds the earliest acceptance time among the messages it covers,
//! which a re-send asks from, and the latest, which the mark expires by. Each message
//! dropped counts with its own acceptance time, a re-sent copy dropped again with its new
//! one.
//!
//! **Cadence.** Once asked, a sender is asked again at most once in any 24 hours while its
//! mark lives, on the device clock. Asking doesn't change the mark's times. A request that
//! doesn't reach the sender's inbox counts as not sent: one still queued is tried again at
//! the next receive, one refused for good, or no longer kept, is asked anew, and the 24 hours
//! count from the last request the outbox delivered.
//!
//! **Expiry and bounds.** A mark goes [`crate::KEPT_MESSAGE_MS`] after its latest time, on the
//! highest service time verified, once its sender keeps none of the messages it covers.
//! Past [`MAX_DROPPED_MARKS`] marks in a family, the mark whose latest time is earliest goes,
//! on a tie the one of the lower sender identity. A mark that goes leaves nothing to ask
//! for.
//!
//! **Backups** carry the marks whole, their times and when they were last asked, so a
//! restore neither loses nor resets them; a restore takes each mark bound to its key's
//! sender.

use pubky_switch_client::app::Delivery;
use pubky_switch_client::app::Protocol as _;
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::records::DAY_MS;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Result};
use crate::paykit::Paykit;
use crate::resend::ResendSelection;
use crate::state::key;

/// How many marks of each family the app keeps (module docs: replies dropped for the bounds,
/// entries dropped unverified, entries dropped while they waited for a scope key): past it,
/// the mark whose latest time is earliest goes, a tie going to the lower sender identity.
pub const MAX_DROPPED_MARKS: usize = 128;

/// A mark of a sender's dropped messages (module docs).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DropMark {
    /// The sender identity.
    #[serde(with = "serde_b64")]
    pub(crate) sender: [u8; 32],
    /// The earliest acceptance time among the messages dropped: what a re-send asks from.
    pub(crate) earliest_ms: u64,
    /// The latest: what the mark expires by.
    pub(crate) latest_ms: u64,
    /// Whether asking is due: for replies, once the app holds a record, of either side,
    /// whose counterparty is the sender, or at once once a stop of the sender's is dropped
    /// for the bounds; for entries dropped unverified, from the start. A mark of key waits
    /// keeps it `false`: it is due while the app holds the keys it waits for
    /// ([`KeyWaitMark`]).
    pub(crate) due: bool,
    /// When the outbox last delivered a request for this mark, on the device clock.
    pub(crate) asked_at_ms: Option<u64>,
    /// A request queued for this mark and not yet seen delivered.
    pub(crate) queued: Option<QueuedRequest>,
}

/// A `resend_request` a mark queued (`DropMark::queued`): its `msg_id`, which the outbox
/// answers its delivery by, and its hash, the proof that the message the outbox delivered
/// under that `msg_id` is this request ([`Paykit::delivered_request`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct QueuedRequest {
    pub(crate) msg_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) hash: [u8; 32],
}

impl DropMark {
    /// Whether the mark has gone by `service`, the highest service time verified: its
    /// latest time plus [`crate::KEPT_MESSAGE_MS`] is at or before it.
    pub(crate) fn expired(&self, service: Option<u64>) -> bool {
        service.is_some_and(|t| self.latest_ms.saturating_add(crate::KEPT_MESSAGE_MS) <= t)
    }

    /// Bring both times down to `bound` where they are above it, as a restore bounds them
    /// (`Paykit::bound_restored_cursor_time`). Whether anything changed.
    pub(crate) fn clamp(&mut self, bound: u64) -> bool {
        if self.latest_ms <= bound {
            return false;
        }
        self.latest_ms = bound;
        self.earliest_ms = self.earliest_ms.min(bound);
        true
    }
}

/// A mark of a sender's entries the client dropped while they waited for a scope key
/// (`kw/<sender>`, module docs): a [`DropMark`] with the data epochs the messages waited
/// for, ascending, each once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeyWaitMark {
    #[serde(flatten)]
    pub(crate) mark: DropMark,
    /// The data epochs the messages it covers waited for.
    pub(crate) epochs: Vec<u64>,
}

impl KeyWaitMark {
    /// The mark with `epoch` among those it waits for.
    fn with_epoch(mark: DropMark, epochs: Vec<u64>, epoch: u64) -> Self {
        let mut epochs = epochs;
        if let Err(at) = epochs.binary_search(&epoch) {
            epochs.insert(at, epoch);
        }
        Self { mark, epochs }
    }
}

/// A mark's value as kept: a [`DropMark`], or one carrying more ([`KeyWaitMark`]).
pub(crate) trait MarkValue: crate::state::Stored + Serialize {
    /// The mark's times, cadence and request.
    fn drop_mark(&mut self) -> &mut DropMark;
}

impl MarkValue for DropMark {
    fn drop_mark(&mut self) -> &mut DropMark {
        self
    }
}

impl MarkValue for KeyWaitMark {
    fn drop_mark(&mut self) -> &mut DropMark {
        &mut self.mark
    }
}

/// The three families of marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MarkFamily {
    /// Replies dropped for the bounds on kept replies.
    Replies,
    /// Entries dropped unverified.
    Unverified,
    /// Entries dropped while they waited for a scope key.
    KeyWaits,
}

impl MarkFamily {
    /// The family's key prefix.
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Replies => key::DROPPED_REPLIES,
            Self::Unverified => key::DROPPED_UNVERIFIED,
            Self::KeyWaits => key::KEY_WAITS,
        }
    }

    /// The key of `sender`'s mark.
    pub(crate) fn key(self, sender: &[u8; 32]) -> String {
        match self {
            Self::Replies => key::dropped_replies(sender),
            Self::Unverified => key::dropped_unverified(sender),
            Self::KeyWaits => key::key_waits(sender),
        }
    }

    /// What a request for `mark` asks to re-send.
    fn selection(self, mark: &DropMark) -> ResendSelection {
        let selection = ResendSelection::default().with_accepted_from_ms(mark.earliest_ms);
        match self {
            Self::Replies => selection,
            Self::Unverified | Self::KeyWaits => selection.with_accepted_until_ms(mark.latest_ms),
        }
    }
}

/// What counting a message from `sender` accepted at `at` does to a family's marks.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Counted {
    /// The sender's mark to put; `None` when the new mark is the one the bound drops.
    pub(crate) put: Option<DropMark>,
    /// The sender of a mark the bound drops for it.
    pub(crate) evicted: Option<[u8; 32]>,
}

/// Count a message from `sender` accepted at `at`, dropped: `held` is the sender's mark if
/// it holds one, `others` every mark of the family as (sender, latest time), read only when
/// it holds none. A new mark is `due` as given. Past [`MAX_DROPPED_MARKS`], the mark whose
/// latest time is earliest goes ([`mark_evicted`]), which may be the new one.
pub(crate) fn count_dropped(
    held: Option<DropMark>,
    others: &[([u8; 32], u64)],
    sender: [u8; 32],
    at: u64,
    due: bool,
) -> Counted {
    if let Some(mut mark) = held {
        mark.earliest_ms = mark.earliest_ms.min(at);
        mark.latest_ms = mark.latest_ms.max(at);
        return Counted {
            put: Some(mark),
            evicted: None,
        };
    }
    let mark = DropMark {
        sender,
        earliest_ms: at,
        latest_ms: at,
        due,
        asked_at_ms: None,
        queued: None,
    };
    let mut all: Vec<([u8; 32], u64)> = others.to_vec();
    all.push((sender, at));
    match mark_evicted(&all) {
        None => Counted {
            put: Some(mark),
            evicted: None,
        },
        Some(gone) if gone == all.len() - 1 => Counted {
            put: None,
            evicted: None,
        },
        Some(gone) => Counted {
            put: Some(mark),
            evicted: Some(all[gone].0),
        },
    }
}

/// Which of `marks`, as (sender, latest time), goes for the bound: `None` within
/// [`MAX_DROPPED_MARKS`]; past it, the one whose latest time is earliest, a tie going to the
/// lower sender identity.
pub(crate) fn mark_evicted(marks: &[([u8; 32], u64)]) -> Option<usize> {
    if marks.len() <= MAX_DROPPED_MARKS {
        return None;
    }
    marks
        .iter()
        .enumerate()
        .min_by_key(|(_, (sender, latest))| (*latest, *sender))
        .map(|(index, _)| index)
}

impl Paykit {
    /// Count a message from `sender` accepted at `at`, dropped unverified, in `tx`: its mark
    /// (`ud/<sender>`), due at once.
    pub(crate) async fn count_dropped_unverified(
        &self,
        tx: &mut crate::state::Tx,
        sender: [u8; 32],
        at: u64,
    ) -> Result<()> {
        let family = MarkFamily::Unverified;
        let held = self.local_get::<DropMark>(&family.key(&sender)).await?;
        let others = match held {
            Some(_) => Vec::new(),
            None => self.marks_of(family).await?,
        };
        let counted = count_dropped(held, &others, sender, at, true);
        if let Some(gone) = counted.evicted {
            tx.delete(&family.key(&gone));
        }
        if let Some(mark) = counted.put {
            tx.put(&family.key(&sender), &mark)?;
        }
        Ok(())
    }

    /// Count a message from `sender` accepted at `at`, dropped while it waited for a scope key
    /// of `epoch`, in `tx`: its mark of key waits (`kw/<sender>`), with the epoch, due only
    /// while the app holds the keys it waits for ([`Paykit::ask_on_marks`]).
    pub(crate) async fn count_key_wait(
        &self,
        tx: &mut crate::state::Tx,
        sender: [u8; 32],
        at: u64,
        epoch: u64,
    ) -> Result<()> {
        let family = MarkFamily::KeyWaits;
        let held = self.local_get::<KeyWaitMark>(&family.key(&sender)).await?;
        let others = match held {
            Some(_) => Vec::new(),
            None => self.marks_of(family).await?,
        };
        let (held, epochs) = match held {
            Some(held) => (Some(held.mark), held.epochs),
            None => (None, Vec::new()),
        };
        let counted = count_dropped(held, &others, sender, at, false);
        if let Some(gone) = counted.evicted {
            tx.delete(&family.key(&gone));
        }
        if let Some(mark) = counted.put {
            tx.put(
                &family.key(&sender),
                &KeyWaitMark::with_epoch(mark, epochs, epoch),
            )?;
        }
        Ok(())
    }

    /// Every mark of `family`, as (sender, latest time).
    async fn marks_of(&self, family: MarkFamily) -> Result<Vec<([u8; 32], u64)>> {
        let mut out = Vec::new();
        match family {
            MarkFamily::KeyWaits => {
                for (_, held) in self.local_scan::<KeyWaitMark>(family.prefix()).await? {
                    out.push((held.mark.sender, held.mark.latest_ms));
                }
            }
            MarkFamily::Replies | MarkFamily::Unverified => {
                for (_, mark) in self.local_scan::<DropMark>(family.prefix()).await? {
                    out.push((mark.sender, mark.latest_ms));
                }
            }
        }
        Ok(out)
    }

    /// Whether the app holds every scope key of `epoch` its certificate is given, of the
    /// scopes the protocol keys.
    fn holds_epoch_keys(&self, epoch: u64) -> bool {
        self.app
            .cert()
            .scopes()
            .iter()
            .filter(|scope| self.protocol.has_keys(scope))
            .all(|scope| self.app.scope_key(epoch, scope).is_some())
    }

    /// Ask on the marks, at a receive (module docs): a reply mark becomes due once the app
    /// holds a record, of either side, whose counterparty is its sender; a mark past its
    /// expiry goes; each due mark asks its sender when its cadence allows. Returns how many
    /// requests were queued.
    pub(crate) async fn ask_on_marks(&mut self) -> Result<u64> {
        self.note_requests_held().await?;
        let mut asked = 0u64;
        for family in [MarkFamily::Replies, MarkFamily::Unverified] {
            for (rest, mark) in self.local_scan::<DropMark>(family.prefix()).await? {
                let due = mark.due;
                asked = asked.saturating_add(self.on_mark(family, &rest, mark, due).await?);
            }
        }
        // A mark of key waits asks only while the app holds every key its messages waited
        // for: a copy sent before would wait for the same key.
        let family = MarkFamily::KeyWaits;
        for (rest, held) in self.local_scan::<KeyWaitMark>(family.prefix()).await? {
            let due = held
                .epochs
                .iter()
                .all(|epoch| self.holds_epoch_keys(*epoch));
            asked = asked.saturating_add(self.on_mark(family, &rest, held, due).await?);
        }
        Ok(asked)
    }

    /// Act on `mark`, of `family` at the key whose rest after the prefix is `rest`: it goes
    /// past its expiry; otherwise, when `due`, it asks its sender (module docs). Returns how
    /// many requests were queued: 1 or 0.
    async fn on_mark<M: MarkValue>(
        &mut self,
        family: MarkFamily,
        rest: &str,
        mut mark: M,
        due: bool,
    ) -> Result<u64> {
        let key = format!("{}{rest}", family.prefix());
        if mark.drop_mark().expired(self.service_time()) {
            self.local_delete(&key).await?;
            return Ok(0);
        }
        let sender = mark.drop_mark().sender;
        // A sender blocked is asked nothing while it is, nor while the copy of the deny list
        // isn't known (Paykit spec 8.1); its mark stays, and asks once it is unblocked, on
        // its cadence, until it expires.
        if !due || self.sends_held(&sender).await? {
            return Ok(0);
        }
        self.ask_on_mark(family, &key, mark).await
    }

    /// Ask the sender of `held`, at `key`, when its cadence allows (module docs). Returns
    /// how many requests were queued: 1 or 0.
    async fn ask_on_mark<M: MarkValue>(
        &mut self,
        family: MarkFamily,
        key: &str,
        mut held: M,
    ) -> Result<u64> {
        let sender = held.drop_mark().sender;
        if let Some(queued) = held.drop_mark().queued.clone() {
            let msg_id = &queued.msg_id;
            if matches!(
                self.app.delivery(msg_id).await?,
                Some(Delivery::Queued { .. })
            ) {
                // Queued, it is tried again now; one that fails stays queued for the next
                // receive.
                let _ = self.app.flush_message(msg_id).await;
            }
            match self.app.delivery(msg_id).await? {
                Some(Delivery::Queued { .. }) => return Ok(0),
                // Delivered counts only once the message delivered is shown to be this
                // request: a `msg_id` naming another message is a corrupt store, never a
                // request delivered.
                Some(Delivery::Accepted { .. }) => {
                    self.delivered_request(&sender, &queued).await?;
                    held.drop_mark().asked_at_ms = Some(self.now());
                }
                // Refused for good, or no longer kept: not sent, so asked anew below.
                _ => {}
            }
            held.drop_mark().queued = None;
            self.local_put(key, &held).await?;
        }
        let now = self.now();
        // A time ahead of the clock (restored, or the clock set back) is due: it never holds
        // the next request back for longer than a day.
        if held
            .drop_mark()
            .asked_at_ms
            .is_some_and(|at| at <= now && now - at < DAY_MS)
        {
            return Ok(0);
        }
        let selection = family.selection(held.drop_mark());
        let signed = self.resend_request_message(sender, selection)?;
        let msg_id = signed.msg_id().to_owned();
        // Kept before it is queued: a crash between the two finds the request not kept by
        // the outbox, and asks anew.
        held.drop_mark().queued = Some(QueuedRequest {
            msg_id: msg_id.clone(),
            hash: signed.hash(),
        });
        self.local_put(key, &held).await?;
        self.app.queue_signed(signed).await?;
        let _ = self.app.flush_message(&msg_id).await;
        if matches!(
            self.app.delivery(&msg_id).await?,
            Some(Delivery::Accepted { .. })
        ) {
            let mark = held.drop_mark();
            mark.queued = None;
            mark.asked_at_ms = Some(self.now());
            self.local_put(key, &held).await?;
        }
        Ok(1)
    }

    /// Check that the message the outbox delivered to `sender` under `queued`'s `msg_id` is
    /// that `resend_request`: one to `sender`, of that kind and hash.
    ///
    /// # Errors
    ///
    /// A corrupt store when none is: the mark points to another message.
    async fn delivered_request(&self, sender: &[u8; 32], queued: &QueuedRequest) -> Result<()> {
        let sent = self.app.sent_to(sender, 0, u64::MAX).await?;
        if sent.iter().any(|message| {
            message.msg_id == queued.msg_id
                && message.kind == paykit_lib::kinds::Kind::ResendRequest.as_str()
                && message.plaintext.hash() == queued.hash
        }) {
            Ok(())
        } else {
            Err(PaykitError::corrupt())
        }
    }

    /// Make due the reply mark of each sender whose record the app holds: any activity record
    /// of either side, read as that side, whose counterparty the mark names, one held before
    /// the reply was dropped included: on the payee's side, the record of a request to the
    /// sender or of the sender's payment without a request, which a payee app's kept replies
    /// wait for; on the payer's, of the sender's request or of a payment to the sender
    /// without a request, which a payer app's kept receipts wait for. The records are read
    /// only while a mark waits, and a failed read fails the pass.
    async fn note_requests_held(&mut self) -> Result<()> {
        use paykit_lib::collections::Collection;
        use paykit_lib::records::ActivitySide;
        let own = self.identity();
        let family = MarkFamily::Replies;
        let mut waiting = std::collections::BTreeMap::new();
        for (rest, mark) in self.local_scan::<DropMark>(family.prefix()).await? {
            let key = format!("{}{rest}", family.prefix());
            if !mark.due {
                waiting.insert(mark.sender, (key, mark));
            }
        }
        if waiting.is_empty() {
            return Ok(());
        }
        let mut tx = crate::state::Tx::default();
        for (collection, side) in [
            (Collection::PayeeActivity, ActivitySide::Payee),
            (Collection::PayerActivity, ActivitySide::Payer),
        ] {
            for held in self.app.records_of(collection.name()).await? {
                let Some(value) = held.content.value() else {
                    continue;
                };
                // A record that doesn't read shows no counterparty: it makes no mark due.
                let Ok(read) = paykit_lib::records::ActivityRecord::read(value, &own, side) else {
                    continue;
                };
                if let Some((key, mut mark)) = waiting.remove(&read.counterparty) {
                    mark.due = true;
                    tx.put(&key, &mark)?;
                }
            }
        }
        self.local_commit(tx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(sender: u8, earliest_ms: u64, latest_ms: u64) -> DropMark {
        DropMark {
            sender: [sender; 32],
            earliest_ms,
            latest_ms,
            due: false,
            asked_at_ms: Some(3),
            queued: None,
        }
    }

    #[test]
    fn test_marks_count_keeps_the_earliest_and_the_latest() {
        let held = mark(1, 100, 200);
        let later = count_dropped(Some(held.clone()), &[], [1; 32], 300, true);
        assert_eq!(later.put, Some(mark(1, 100, 300)));
        let earlier = count_dropped(Some(held.clone()), &[], [1; 32], 50, true);
        assert_eq!(earlier.put, Some(mark(1, 50, 200)));
        let within = count_dropped(Some(held.clone()), &[], [1; 32], 150, true);
        assert_eq!(within.put, Some(held));
        assert_eq!(within.evicted, None);
    }

    #[test]
    fn test_marks_count_a_new_mark_at_its_one_time() {
        let counted = count_dropped(None, &[([2; 32], 9)], [1; 32], 70, true);
        let put = counted.put.unwrap();
        assert_eq!((put.earliest_ms, put.latest_ms, put.due), (70, 70, true));
        assert_eq!((put.asked_at_ms, put.queued), (None, None));
        assert_eq!(counted.evicted, None);
    }

    #[test]
    fn test_marks_expire_by_the_latest_time() {
        let held = mark(1, 100, 200);
        assert!(!held.expired(None));
        assert!(!held.expired(Some(200 + crate::KEPT_MESSAGE_MS - 1)));
        assert!(held.expired(Some(200 + crate::KEPT_MESSAGE_MS)));
    }

    #[test]
    fn test_marks_evicted_none_within_the_bound() {
        let marks: Vec<([u8; 32], u64)> = (0..MAX_DROPPED_MARKS)
            .map(|i| ([u8::try_from(i).unwrap(); 32], 10))
            .collect();
        assert_eq!(mark_evicted(&marks), None);
    }

    #[test]
    fn test_marks_evicted_the_earliest_latest_past_the_bound() {
        let mut marks: Vec<([u8; 32], u64)> = (0..=MAX_DROPPED_MARKS)
            .map(|i| {
                (
                    [u8::try_from(i % 250).unwrap(); 32],
                    100 + u64::try_from(i).unwrap(),
                )
            })
            .collect();
        marks.swap(0, 50);
        assert_eq!(mark_evicted(&marks), Some(50));
    }

    #[test]
    fn test_marks_evicted_tie_goes_to_the_lower_identity() {
        let mut marks = vec![([9; 32], 5), ([3; 32], 5)];
        marks.extend((0..MAX_DROPPED_MARKS - 1).map(|_| ([200; 32], 50)));
        assert_eq!(mark_evicted(&marks), Some(1));
    }

    #[test]
    fn test_marks_count_past_the_bound_drops_the_earliest_or_the_new() {
        let full: Vec<([u8; 32], u64)> = (0..MAX_DROPPED_MARKS)
            .map(|i| {
                (
                    [u8::try_from(i + 10).unwrap(); 32],
                    100 + u64::try_from(i).unwrap(),
                )
            })
            .collect();
        let later = count_dropped(None, &full, [1; 32], 1_000, true);
        assert!(later.put.is_some());
        assert_eq!(later.evicted, Some([10; 32]));
        let earliest = count_dropped(None, &full, [1; 32], 5, true);
        assert_eq!(
            earliest,
            Counted {
                put: None,
                evicted: None
            }
        );
    }
}
