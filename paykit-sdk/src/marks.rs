//! Marks of a sender's messages dropped before they were taken, and the re-sends they ask
//! for (Paykit spec 8.1, "Replies before their record" and "Stored entries"; Paykit spec 9).
//!
//! Four families, each with one mark per sender identity, the identity its certificate
//! names:
//!
//! - **Replies dropped for the bounds on kept replies** (`pd/<sender>`, `receive.rs`): a
//!   payer's replies at a payee app, and receipts and period details at a payer app. The
//!   app asks the sender to re-send from the mark's earliest acceptance time as soon as
//!   sending is allowed, including while the record is missing. A payee's cancellation
//!   dropped for the bound on stops counts in the same mark. The step that drops a kept
//!   reply forgets its processed pair so that a re-sent copy can be taken again.
//! - **Entries dropped unverified** (`ud/<sender>`, `stored.rs`). The app asks that sender
//!   at once to re-send what its inbox accepted from the mark's earliest time to its latest
//!   (`accepted_from` and `accepted_until`).
//! - **Entries dropped while they waited for a scope key** (`kw/<sender>`, `stored.rs`): the
//!   client drops such an entry past its bounds unprocessed (Switch spec 8.2). The mark also
//!   holds the data epochs the messages waited for, and asks that sender, as for entries
//!   dropped unverified, only while the app holds every key of those epochs its certificate
//!   is given: a copy sent before would wait for the same key.
//!
//! - **Messages dropped for the user's block** (`bd/<sender>`, `receive.rs`): a message
//!   that would create state from a sender the user blocked (the client's view of the deny
//!   list, this app's own block) is dropped as one never processed. The step that counts it
//!   names its pair in the mark, and the pass after the read forgets it
//!   ([`Paykit::forget_blocked_pairs`]). The mark asks the sender to re-send what its inbox
//!   accepted from the mark's earliest time to its latest only once every pair it names is
//!   forgotten, no block or unblock of the sender this app wrote is still to be shown by the
//!   lists, and a read of the lists made in the same pass, after the receive's steps, shows
//!   the sender neither blocked nor flagged: the ask reads the lists itself, so it never
//!   judges a sender by a copy older than the block the step saw. A read that fails, or a
//!   copy of the flagged senders that isn't kept, asks nothing. A mark covers one window:
//!   once an ask for it is delivered, the next message counted starts a new one. What can
//!   no longer come back is told to the host, once per sender and window
//!   ([`crate::NoticeKind::BlockedMessagesLost`]): a window that expires before an ask for it
//!   was delivered, the part of a window its sender no longer keeps at the window's first
//!   ask, and a window the bound drops before it asked. A request such a mark names as queued
//!   that the outbox shows delivered before the mark's expiry counts as its ask, as at expiry;
//!   one the bound drops in a receive step owes its notice until the pass after the read has
//!   read that delivery ([`LostOwed`]). A restore and a `missed` item widen
//!   these marks for counterparties on the deny list at its next read. A missed item's
//!   window (`bw/`) applies regardless of the restore verdict. A restore keeps its first
//!   read separately (`rh`) and applies it only if the held cursor is ahead or unknown,
//!   including when a later page ends a clean verdict (Paykit spec 7.2 and 8.1).
//!
//! A payer applies the identity's shared block floor when it closes a blocked window or
//! applies a widening (Paykit spec 8.1 and `crate::floor`). Receiving a blocked message,
//! retiring its mark and sending its recovery request leave the shared floor alone.
//!
//! **Two times.** A mark holds the earliest acceptance time among the messages it covers,
//! which a re-send asks from, and the latest, which the mark expires by. Each message
//! dropped counts with its own acceptance time, a re-sent copy dropped again with its new
//! one.
//!
//! **Cadence.** Once asked, a sender is asked again at most once in any 24 hours while its
//! mark lives, on the device clock. Asking doesn't change the mark's times. A request that
//! doesn't reach the sender's inbox counts as not sent: one still queued is tried again at
//! the next receive. A request for dropped replies can be replaced after confirmed first
//! non-delivery. If the outcome is uncertain, overlapping asks wait for reconciliation.
//! The 24 hours count from the last request the outbox delivered.
//!
//! **Expiry and bounds.** A mark goes [`crate::KEPT_MESSAGE_MS`] after its latest time, on the
//! highest service time verified, once its sender keeps none of the messages it covers.
//! Past [`MAX_DROPPED_MARKS`] marks in a family, the mark whose latest time is earliest goes,
//! on a tie the one of the lower sender identity. A mark that goes leaves nothing to ask
//! for.
//!
//! **Backups** carry the marks whole, their times and when they were last asked, so a
//! restore neither loses nor resets them; a restore takes each mark bound to its key's
//! sender. They carry the notices owed too ([`LostOwed`]), and the client's backup carries
//! the outbox's records under their `msg_id`s, not its sent messages: the restored outbox
//! holds the request a notice names as the backup's outbox did, and the pass after the read
//! settles the notice from its answer. A request delivered within the window after the
//! restore tells nothing; one still queued, refused, or delivered before the backup and gone
//! from the outbox then tells the window lost.

use pubky_switch_client::app::Delivery;
use pubky_switch_client::app::Protocol as _;
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::records::DAY_MS;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::resend::ResendSelection;
use crate::state::key;

/// How many marks of each family the app keeps (module docs: replies dropped for the bounds,
/// entries dropped unverified, entries dropped while they waited for a scope key, messages
/// dropped for the user's block): past it, the mark whose latest time is earliest goes, a tie
/// going to the lower sender identity. For a block's mark, a speculative one goes first, and
/// one with pairs still to forget never goes.
pub const MAX_DROPPED_MARKS: usize = 128;

/// The most flagged senders the app keeps a copy of (`fg`): past it, the copy isn't kept,
/// and the flagged state of every sender counts as not known, which holds every ask of a
/// block mark.
pub const MAX_FLAGGED_COPY: usize = 4096;

/// A missed item's acceptance window and the floor covered by its original feed page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BlockedWindow {
    pub(crate) from_ms: u64,
    pub(crate) until_ms: u64,
    pub(crate) floor_ms: u64,
}

/// Before a list read each missed callback owns one window. That read keeps one copy
/// of its deny list for all windows it observed, including when the shared floor must retry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MissedWidening {
    pub(crate) id: crate::hold::HoldEpoch,
    pub(crate) windows: Vec<BlockedWindow>,
    pub(crate) deny: Option<Vec<[u8; 32]>>,
}

/// The first list read after a restore, used if the restore requires a block window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WideningRead {
    pub(crate) at_ms: u64,
    pub(crate) deny: Vec<[u8; 32]>,
}

/// A mark of a sender's dropped messages (module docs).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DropMark {
    /// The sender identity.
    #[serde(with = "serde_b64")]
    pub(crate) sender: [u8; 32],
    /// The earliest acceptance time among the messages dropped: what a re-send asks from.
    pub(crate) earliest_ms: u64,
    /// The latest: what the mark expires by.
    pub(crate) latest_ms: u64,
    /// Whether the mark was recorded as due. Reply marks ask as soon as sends are
    /// permitted. A key wait is due when the app holds its keys ([`KeyWaitMark`]).
    pub(crate) due: bool,
    /// When the outbox last delivered a request for this mark, on the device clock.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) asked_at_ms: Option<u64>,
    /// A request queued for this mark and not yet seen delivered.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) queued: Option<QueuedRequest>,
    /// For a block's mark: the messages counted whose processed pair is still to be
    /// forgotten, outside the receive step that counted them
    /// ([`Paykit::forget_blocked_pairs`]); the mark asks only once none is left, so the copies
    /// its request brings back aren't dropped as duplicates. At most
    /// [`MAX_PENDING_FORGETS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) pending_forget: Vec<String>,
    /// For a block's mark: its window comes only from a restore's or a `missed` item's
    /// widening, no message counted in it. The bound drops such a mark first.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) speculative: bool,
    /// For a block's mark: the part of its window its sender no longer kept at the window's
    /// first ask was told to the host, once.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) late_told: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The most messages a block's mark holds whose pair is still to be forgotten: past it, the
/// receive step holds the next entry from that sender until the pairs are forgotten.
pub const MAX_PENDING_FORGETS: usize = 64;

/// A `resend_request` a mark queued (`DropMark::queued`): its `msg_id`, which the outbox
/// answers its delivery by, and its hash, the proof that the message the outbox delivered
/// under that `msg_id` is this request ([`Paykit::delivered_request`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueuedRequest {
    pub(crate) msg_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) hash: [u8; 32],
}

impl QueuedRequest {
    fn matches_signed_request(&self, kind: &str, hash: [u8; 32]) -> bool {
        kind == paykit_lib::kinds::Kind::ResendRequest.as_str() && self.hash == hash
    }
}

/// The most notices of lost messages the app keeps owed (`bl/`, [`LostOwed`]): past it, the
/// receive step holds the next entry whose count would drop a mark that owes one, until the
/// pass after the read settles those kept. None is dropped for this bound.
pub const MAX_LOST_OWED: usize = 64;

/// The notice owed on a block's mark the receive step dropped for the bound before an ask
/// for it was seen delivered, while the mark still named a request as queued
/// (`bl/<sender>/<msg_id>`). The receive step makes no client reads, so the outbox's answer
/// on that request is read in the pass after the read ([`Paykit::settle_lost_owed`]): one
/// delivered before the mark's expiry tells nothing, as at expiry; any other tells
/// [`crate::NoticeKind::BlockedMessagesLost`] for the window, raised at `raised_ms`. Written
/// in the commit that drops the mark, so a later delivery can settle the notice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LostOwed {
    /// The dropped mark's sender identity.
    #[serde(with = "serde_b64")]
    pub(crate) sender: [u8; 32],
    /// The dropped mark's earliest time: where the window told lost starts.
    pub(crate) earliest_ms: u64,
    /// Its latest time: where that window ends, and what the delivery is judged in time by.
    pub(crate) latest_ms: u64,
    /// The time the notice carries as raised: the receive step's, as when told at once.
    pub(crate) raised_ms: u64,
    /// The `msg_id` of the request the mark named as queued.
    pub(crate) msg_id: String,
    /// That request's hash, the proof that the message the outbox delivered under the
    /// `msg_id` is this request ([`Paykit::delivered_request`]).
    #[serde(with = "serde_b64")]
    pub(crate) hash: [u8; 32],
}

impl LostOwed {
    /// The notice owed on `gone`, dropped with `queued` named as queued, raised at `raised`.
    pub(crate) fn of(gone: &DropMark, queued: &QueuedRequest, raised: u64) -> Self {
        Self {
            sender: gone.sender,
            earliest_ms: gone.earliest_ms,
            latest_ms: gone.latest_ms,
            raised_ms: raised,
            msg_id: queued.msg_id.clone(),
            hash: queued.hash,
        }
    }
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
#[serde(deny_unknown_fields)]
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

/// The four families of marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MarkFamily {
    /// Replies dropped for the bounds on kept replies.
    Replies,
    /// Entries dropped unverified.
    Unverified,
    /// Entries dropped while they waited for a scope key.
    KeyWaits,
    /// Messages dropped for the user's block.
    Blocked,
}

impl MarkFamily {
    /// The family's key prefix.
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Replies => key::DROPPED_REPLIES,
            Self::Unverified => key::DROPPED_UNVERIFIED,
            Self::KeyWaits => key::KEY_WAITS,
            Self::Blocked => key::DROPPED_BLOCKED,
        }
    }

    /// The key of `sender`'s mark.
    pub(crate) fn key(self, sender: &[u8; 32]) -> String {
        match self {
            Self::Replies => key::dropped_replies(sender),
            Self::Unverified => key::dropped_unverified(sender),
            Self::KeyWaits => key::key_waits(sender),
            Self::Blocked => key::dropped_blocked(sender),
        }
    }

    /// What a request for `mark` asks to re-send.
    fn selection(self, mark: &DropMark) -> ResendSelection {
        let selection = ResendSelection::default().with_accepted_from_ms(mark.earliest_ms);
        match self {
            Self::Replies => selection,
            Self::Unverified | Self::KeyWaits | Self::Blocked => {
                selection.with_accepted_until_ms(mark.latest_ms)
            }
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
        pending_forget: Vec::new(),
        speculative: false,
        late_told: false,
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

/// What counting a window from `from` to `until` does to the block marks
/// ([`count_blocked`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BlockedCount {
    /// The sender's mark to put, and the mark the bound drops for it, if any.
    Put {
        mark: Box<DropMark>,
        evicted: Option<Box<DropMark>>,
    },
    /// The new mark is the one the bound drops: a speculative one, past the bound with no
    /// speculative mark to drop before it.
    Dropped,
    /// No mark may go: every one the bound could drop still names pairs to forget, so the
    /// message counted must wait (`receive.rs`), or the widening be applied again later.
    Held,
}

/// Count, in the sender's block mark, a window from `from` to `until`: a message dropped for
/// the user's block (`real`, from = until = its acceptance time), or a restore's or a
/// `missed` item's widening. `held` is the sender's mark if it holds one, `others` every
/// block mark, read only when it holds none.
///
/// - A held mark whose window an ask was delivered for starts a new window; otherwise its
///   window widens. A request queued for it and not yet seen delivered is forgotten: the
///   next ask covers the whole window.
/// - A new mark past [`MAX_DROPPED_MARKS`] takes the place of another. Only a mark with no
///   pair left to forget may go; a speculative one goes before a real one, then the one
///   whose latest time is earliest, a tie going to the lower sender identity. A real
///   message's new mark is never the one dropped: it waits instead ([`BlockedCount::Held`])
///   while no other may go. A speculative new mark may be the one dropped.
pub(crate) fn count_blocked(
    held: Option<DropMark>,
    others: &[DropMark],
    sender: [u8; 32],
    window: (u64, u64),
    real: bool,
) -> BlockedCount {
    let (from, until) = window;
    if let Some(mut mark) = held {
        if mark.asked_at_ms.is_some() {
            // A new window, which never lowers the mark's latest time: a request of the old
            // window may still be on its way back, and is judged by it.
            mark.earliest_ms = from;
            mark.latest_ms = mark.latest_ms.max(until);
            mark.asked_at_ms = None;
            mark.late_told = false;
            mark.speculative = !real;
        } else {
            mark.earliest_ms = mark.earliest_ms.min(from);
            mark.latest_ms = mark.latest_ms.max(until);
            mark.speculative &= !real;
        }
        mark.queued = None;
        return BlockedCount::Put {
            mark: Box::new(mark),
            evicted: None,
        };
    }
    let mark = DropMark {
        sender,
        earliest_ms: from,
        latest_ms: until,
        due: true,
        asked_at_ms: None,
        queued: None,
        pending_forget: Vec::new(),
        speculative: !real,
        late_told: false,
    };
    if others.len() < MAX_DROPPED_MARKS {
        return BlockedCount::Put {
            mark: Box::new(mark),
            evicted: None,
        };
    }
    let rank = |m: &DropMark| (!m.speculative, m.latest_ms, m.sender);
    let mut candidates: Vec<&DropMark> = others
        .iter()
        .filter(|m| m.pending_forget.is_empty())
        .collect();
    if !real {
        candidates.push(&mark);
    }
    match candidates.into_iter().min_by_key(|m| rank(m)) {
        None => BlockedCount::Held,
        Some(chosen) if chosen.sender == sender => BlockedCount::Dropped,
        Some(chosen) => {
            let evicted = Some(Box::new(chosen.clone()));
            BlockedCount::Put {
                mark: Box::new(mark),
                evicted,
            }
        }
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
            MarkFamily::Replies | MarkFamily::Unverified | MarkFamily::Blocked => {
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

    /// Ask on live marks at a receive, subject to the sender's holds and the mark's cadence.
    /// Returns how many requests were queued.
    pub(crate) async fn ask_on_marks(&mut self, refused: &mut Option<Refusal>) -> Result<u64> {
        self.reconcile_reply_asks().await?;
        let mut asked = 0u64;
        for family in [MarkFamily::Replies, MarkFamily::Unverified] {
            for (rest, mark) in self.local_scan::<DropMark>(family.prefix()).await? {
                let due = family == MarkFamily::Replies || mark.due;
                asked =
                    asked.saturating_add(self.on_mark(family, &rest, mark, due, refused).await?);
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
            asked = asked.saturating_add(self.on_mark(family, &rest, held, due, refused).await?);
        }
        asked = asked.saturating_add(Box::pin(self.ask_on_block_marks(refused)).await?);
        Ok(asked)
    }

    /// Ask on the block marks (module docs). A mark asks only once every pair it names is
    /// forgotten and no block or unblock of its sender this app wrote is still to be shown
    /// by the lists (`bk/`): asking for a blocked sender's messages is the permissive
    /// direction, which no mark about an own write decides. Then the lists are read, once,
    /// after this receive's steps: the step judged the block by the client's view of the
    /// sender, which a copy read before may not show, so no older copy decides the ask. The
    /// read's copy must show the sender neither blocked nor flagged: a copy sent to a flagged
    /// sender's re-send would be counted only, and lost. A read that fails, or a copy of the
    /// flagged senders that isn't kept, asks nothing. No time of the device is compared with
    /// a service time. The copy kept from a read under [`crate::LISTS_REFRESH_MS`] old only
    /// spares the read: while it shows every such sender denied or flagged, the read isn't
    /// made, as its answer could only hold the ask. It never decides that a mark asks.
    async fn ask_on_block_marks(&mut self, refused: &mut Option<Refusal>) -> Result<u64> {
        let family = MarkFamily::Blocked;
        let now = self.now();
        let fresh_copy = self
            .local_get::<u64>(key::LISTS_READ)
            .await?
            .is_some_and(|read| crate::receive::lists_read_is_fresh(read, now));
        let kept_flagged = self.local_get::<Vec<[u8; 32]>>(key::FLAGGED).await?;
        // Whether any mark needs the read: one ready to ask that a fresh copy doesn't hold.
        let mut needs_read = false;
        for (_, mark) in self.local_scan::<DropMark>(family.prefix()).await? {
            if !self.block_mark_ready(&mark).await? {
                continue;
            }
            // A fresh copy that still holds the sender spares the read for it, as does a floor
            // raise owed that holds its ask while no read is due for that raise, such as one the
            // divergence hold keeps from being written (`Paykit::raise_owed`).
            let held_by_fresh_copy = fresh_copy
                && (self.denied_as_last_read(&mark.sender).await?
                    || kept_flagged
                        .as_ref()
                        .is_some_and(|flagged| flagged.contains(&mark.sender))
                    || (self.raise_holds_asks_to(&mark.sender).await?
                        && !self.raise_owed().await?));
            needs_read |= !held_by_fresh_copy;
        }
        let read = needs_read && Box::pin(self.read_lists_now()).await?;
        let flagged = if read {
            self.local_get::<Vec<[u8; 32]>>(key::FLAGGED).await?
        } else {
            None
        };
        // The read changes marks: a widening it applies extends a blocked sender's window or
        // adds a mark, and the bound on marks may drop one (`Paykit::apply_block_widenings`).
        // So the marks are read again after it, and each is judged as it is kept now: a mark
        // captured before the read is never expired, asked on or written back.
        let mut asked = 0u64;
        for (rest, mark) in self.local_scan::<DropMark>(family.prefix()).await? {
            let due = self.block_mark_ready(&mark).await?
                && flagged
                    .as_ref()
                    .is_some_and(|flagged| !flagged.contains(&mark.sender));
            asked = asked.saturating_add(self.on_mark(family, &rest, mark, due, refused).await?);
        }
        Ok(asked)
    }

    /// Whether a block's mark is ready to ask, as it is kept now: every pair it names is
    /// forgotten, no block or unblock of its sender this app wrote is still to be shown by
    /// the lists (`bk/`), it would send now (a request queued, or none delivered in the last
    /// 24 hours), and it isn't past its expiry, which only lets it go.
    async fn block_mark_ready(&self, mark: &DropMark) -> Result<bool> {
        let now = self.now();
        let own_change = self
            .local_get::<crate::state::OwnBlockMark>(&key::own_block(&mark.sender))
            .await?
            .is_some();
        let would_send = mark.queued.is_some()
            || mark
                .asked_at_ms
                .is_none_or(|at| at > now || now - at >= DAY_MS);
        Ok(mark.pending_forget.is_empty()
            && !own_change
            && would_send
            && !mark.expired(self.service_time()))
    }

    /// Forget the processed pair of every message a block's mark counted (module docs), each
    /// with the mark's update in one commit ([`pubky_switch_client::app::App::forget_processed`]):
    /// from then on a re-sent copy is a new entry. One the client won't forget yet (an entry
    /// it stores, or an effect it keeps, holds it) stays for a later receive.
    ///
    /// # Errors
    ///
    /// A store failure or an alarm.
    pub(crate) async fn forget_blocked_pairs(&mut self) -> Result<()> {
        use pubky_switch_client::app::{AppError, Refusal as ClientRefusal};
        let family = MarkFamily::Blocked;
        for (rest, mut mark) in self.local_scan::<DropMark>(family.prefix()).await? {
            let key = format!("{}{rest}", family.prefix());
            while let Some(msg_id) = mark.pending_forget.first().cloned() {
                let mut next = mark.clone();
                next.pending_forget.remove(0);
                let write = crate::state::local_put(&key, &next)?;
                match self
                    .app
                    .forget_processed(crate::protocol::BACKUP_NAME, mark.sender, &msg_id, &[write])
                    .await
                {
                    Ok(_) => mark = next,
                    Err(AppError::Refused(ClientRefusal::NotNow(_))) => break,
                    Err(error @ (AppError::Store(_) | AppError::Alarm(_))) => {
                        return Err(error.into())
                    }
                    Err(_) => break,
                }
            }
        }
        Ok(())
    }

    /// Apply the widenings of the block marks owed (`bw/`), once the deny list has just been
    /// read (`deny`): every counterparty it holds is marked from each widening's start to
    /// its end (module docs), and the widening goes, in one commit.
    pub(crate) async fn apply_block_widenings(
        &mut self,
        deny: &[[u8; 32]],
        verified: Option<u64>,
    ) -> Result<()> {
        let now = verified.unwrap_or_default();
        if let Some(mut hold) = self
            .local_get::<crate::hold::RestoreHold>(key::RESTORE_HOLD)
            .await?
        {
            let first_read = || -> Result<WideningRead> {
                Ok(WideningRead {
                    at_ms: verified.ok_or_else(crate::floor::unreadable_floor)?,
                    deny: deny.to_vec(),
                })
            };
            let mut changed = false;
            if let Some(from_ms) = hold.earlier_unread_from_ms.take() {
                let owed = crate::hold::OwedWidening {
                    from_ms: from_ms.min(now),
                    read: first_read()?,
                };
                hold.earlier_widening = Some(match hold.earlier_widening.take() {
                    Some(previous) => previous.covering(owed),
                    None => owed,
                });
                changed = true;
            }
            if let (Some(from_ms), None) = (hold.widening_from_ms, &hold.widening_read) {
                hold.widening_read = Some(first_read()?);
                hold.widening_from_ms = Some(from_ms.min(now));
                changed = true;
            }
            // Keep the first read before marking anyone. A failed write resumes against the
            // same list, even when the host changes its contacts before retrying.
            if changed {
                let mut tx = crate::state::Tx::default();
                tx.put(key::RESTORE_HOLD, &hold)?;
                self.local_commit(tx).await?;
            }
            if hold.earlier_widening.is_some() || hold.widening_owed().is_some() {
                self.local_delete(key::LISTS_READ).await?;
                if self.holds("paykit_pay") && self.first_read_pending() {
                    return Err(crate::floor::unreadable_floor());
                }
            }
            if let Some(owed) = hold.earlier_widening.take() {
                let mut tx = crate::state::Tx::default();
                tx.put(key::RESTORE_HOLD, &hold)?;
                if !self.commit_restore_widening(&owed, tx).await? {
                    return Ok(());
                }
            }
            if let Some(owed) = hold.widening_owed() {
                hold.widening_from_ms = None;
                hold.widening_read = None;
                let mut tx = crate::state::Tx::default();
                tx.put(key::RESTORE_HOLD, &hold)?;
                if !self.commit_restore_widening(&owed, tx).await? {
                    return Ok(());
                }
            }
        }
        let widenings = self
            .local_scan::<MissedWidening>(key::BLOCKED_WIDENINGS)
            .await?;
        for (_, mut widening) in widenings {
            let deny = widening
                .deny
                .clone()
                .ok_or_else(crate::error::PaykitError::corrupt)?;
            let key = key::blocked_widening(widening.id.as_bytes());
            if deny.is_empty() {
                self.local_delete(&key).await?;
                continue;
            }
            self.local_delete(key::LISTS_READ).await?;
            while let Some(window) = widening.windows.pop() {
                let mut tx = crate::state::Tx::default();
                if widening.windows.is_empty() {
                    tx.delete(&key);
                } else {
                    tx.put(&key, &widening)?;
                }
                if self
                    .stage_widening(&deny, window.from_ms, window.until_ms, &mut tx)
                    .await?
                {
                    if self.holds("paykit_pay") && self.first_read_pending() {
                        return Err(crate::floor::unreadable_floor());
                    }
                    if !raised(self.commit_with_floor(window.floor_ms, tx).await)? {
                        return Ok(());
                    }
                } else {
                    self.local_commit(tx).await?;
                }
            }
        }
        Ok(())
    }

    /// Retain the first list that follows each missed item before any floor write.
    pub(crate) async fn keep_missed_list(&mut self, deny: &[[u8; 32]]) -> Result<()> {
        let mut pending = self
            .local_scan::<MissedWidening>(key::BLOCKED_WIDENINGS)
            .await?
            .into_iter()
            .map(|(_, widening)| widening)
            .filter(|widening| widening.deny.is_none());
        let Some(mut group) = pending.next() else {
            return Ok(());
        };
        let mut tx = crate::state::Tx::default();
        for other in pending {
            tx.delete(&key::blocked_widening(other.id.as_bytes()));
            group.windows.extend(other.windows);
        }
        group.deny = Some(deny.to_vec());
        tx.put(&key::blocked_widening(group.id.as_bytes()), &group)?;
        self.local_commit(tx).await
    }

    /// A restore that lost entries raises the floor even when its list was empty. Whether
    /// the widening applied: one whose raise can't be written stays owed ([`raised`]).
    async fn commit_restore_widening(
        &mut self,
        owed: &crate::hold::OwedWidening,
        mut tx: crate::state::Tx,
    ) -> Result<bool> {
        self.stage_widening(&owed.read.deny, owed.from_ms, owed.read.at_ms, &mut tx)
            .await?;
        raised(self.commit_with_floor(owed.read.at_ms, tx).await)
    }

    /// Return whether this window made a mark. A missed item whose speculative marks
    /// were all dropped at the bound records their notices without raising the floor.
    async fn stage_widening(
        &self,
        deny: &[[u8; 32]],
        from: u64,
        until: u64,
        tx: &mut crate::state::Tx,
    ) -> Result<bool> {
        let family = MarkFamily::Blocked;
        let mut marks: Vec<DropMark> = self
            .local_scan::<DropMark>(family.prefix())
            .await?
            .into_iter()
            .map(|(_, mark)| mark)
            .collect();
        let raised = self.service_time().unwrap_or(0);
        let mut marked = false;
        for &sender in deny {
            let existing = marks
                .iter()
                .position(|mark| mark.sender == sender)
                .map(|index| marks.swap_remove(index));
            match count_blocked(existing, &marks, sender, (from, until), false) {
                BlockedCount::Held => {
                    return Err(Refusal::NotNow("the block marks are full".into()).into())
                }
                BlockedCount::Dropped => {
                    tx.put(
                        &key::blocked_lost(&sender, from),
                        &crate::notices::KeptNotice::blocked_lost(sender, from, until, raised),
                    )?;
                }
                BlockedCount::Put { mark, evicted } => {
                    if let Some(mut gone) = evicted {
                        if gone.asked_at_ms.is_none() {
                            gone.asked_at_ms = self.delivered_in_time(&gone).await?;
                        }
                        tell_lost_if_unasked(tx, &gone, raised)?;
                        tx.delete(&family.key(&gone.sender));
                        marks.retain(|m| m.sender != gone.sender);
                    }
                    tx.put(&family.key(&sender), &*mark)?;
                    marks.push(*mark);
                    marked = true;
                }
            }
        }
        Ok(marked)
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
        refused: &mut Option<Refusal>,
    ) -> Result<u64> {
        let key = format!("{}{rest}", family.prefix());
        if mark.drop_mark().expired(self.service_time()) {
            let mut tx = crate::state::Tx::default();
            tx.delete(&key);
            // A block's window that expires before an ask for it was delivered: its messages
            // can no longer come back. One asked for in time tells nothing, the request the
            // mark still names as queued included, once the outbox shows it delivered before
            // the mark expired ([`Paykit::delivered_in_time`]): the write of the delivery
            // into the mark may not have been made, as after a crash.
            if family == MarkFamily::Blocked {
                let mut gone = mark.drop_mark().clone();
                if gone.asked_at_ms.is_none() {
                    gone.asked_at_ms = self.delivered_in_time(&gone).await?;
                }
                tell_lost_if_unasked(&mut tx, &gone, self.service_time().unwrap_or(0))?;
            }
            if family == MarkFamily::Replies {
                let gone = mark.drop_mark();
                if !self
                    .record_expired_reply_loss(
                        &mut tx,
                        &gone.sender,
                        gone.earliest_ms,
                        gone.latest_ms,
                    )
                    .await?
                {
                    return Ok(0);
                }
            }
            self.local_commit(tx).await?;
            return Ok(0);
        }
        let sender = mark.drop_mark().sender;
        // A sender blocked is asked nothing while it is, nor while the copy of the deny list
        // isn't known (Paykit spec 8.1); its mark stays, and asks once it is unblocked, on
        // its cadence, until it expires.
        if !due || self.resend_held(&sender, refused).await? {
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
        let mark = held.drop_mark();
        let sender = mark.sender;
        let selection = family.selection(mark);
        let pending = mark.queued.as_ref().map(|q| (q.msg_id.as_str(), q.hash));
        if !self
            .recovery_request_permitted(&sender, &selection.request(), pending)
            .await?
        {
            return Ok(0);
        }
        if let Some(queued) = held.drop_mark().queued.clone() {
            let msg_id = &queued.msg_id;
            if matches!(
                self.app.delivery(msg_id).await?,
                Some(Delivery::Queued { .. })
            ) {
                // A corrupt mark must not flush another signed message.
                let pending = self
                    .app
                    .queued_messages(crate::protocol::BACKUP_NAME)
                    .await?;
                if !pending
                    .iter()
                    .find(|message| &message.msg_id == msg_id)
                    .is_some_and(|message| {
                        message.to == sender
                            && queued.matches_signed_request(&message.kind, message.hash)
                    })
                {
                    return Err(PaykitError::corrupt());
                }
                let _ = self.send_message(msg_id).await;
            }
            match self.app.delivery(msg_id).await? {
                Some(Delivery::Queued { .. }) => return Ok(0),
                // Delivered counts only once the message delivered is shown to be this
                // request: a `msg_id` naming another message is a corrupt store, never a
                // request delivered.
                Some(Delivery::Accepted { .. }) => {
                    self.delivered_request(&sender, &queued).await?;
                    if family == MarkFamily::Replies {
                        self.reconcile_reply_asks().await?;
                        return Ok(0);
                    }
                    held.drop_mark().asked_at_ms = Some(self.now());
                }
                // A reply ask waits for its exact first-delivery duty. A later refusal,
                // or an absent queue entry after a crash, cannot establish non-delivery.
                _ if family == MarkFamily::Replies => return Ok(0),
                // Other marks follow their usual retry rule.
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
        // A block's window asks only for what its sender still keeps: at the window's first
        // ask, the part older than that can no longer come back, and the host is told, once.
        if family == MarkFamily::Blocked {
            if let Some(service) = self.service_time() {
                let kept_from = service
                    .saturating_sub(crate::KEPT_MESSAGE_MS)
                    .saturating_add(pubky_switch_core::records::CLOCK_TOLERANCE_MS);
                let mark = held.drop_mark();
                if mark.asked_at_ms.is_none() && !mark.late_told && mark.earliest_ms < kept_from {
                    let lost = (mark.earliest_ms, kept_from.min(mark.latest_ms));
                    mark.earliest_ms = lost.1;
                    mark.late_told = true;
                    let mut tx = crate::state::Tx::default();
                    tx.put(
                        &key::blocked_lost(&sender, lost.0),
                        &crate::notices::KeptNotice::blocked_lost(sender, lost.0, lost.1, service),
                    )?;
                    tx.put(key, &held)?;
                    self.local_commit(tx).await?;
                }
            }
        }
        let mark = held.drop_mark();
        let mut selection = family.selection(mark);
        if family == MarkFamily::Replies {
            return self.queue_reply_ask(mark.clone(), selection).await;
        }
        // A missed item can owe both the blocked window and everything from that
        // same start. Give both duties the same signed request and delivery outcome.
        let ask_key = key::ask(&sender, mark.earliest_ms);
        let mut covering = None;
        if family == MarkFamily::Blocked {
            covering = self
                .local_get::<crate::protocol::QueuedAsk>(&ask_key)
                .await?
                .filter(|ask| ask.queued.is_none() && ask.until_ms.is_none());
            if covering.is_some() {
                selection = ResendSelection::default().with_accepted_from_ms(mark.earliest_ms);
                if !self
                    .recovery_request_permitted(&sender, &selection.request(), None)
                    .await?
                {
                    return Ok(0);
                }
            }
        }
        let signed = self.resend_request_message(sender, selection)?;
        let msg_id = signed.msg_id().to_owned();
        let queued = QueuedRequest {
            msg_id: msg_id.clone(),
            hash: signed.hash(),
        };
        let mut tx = crate::state::Tx::default();
        if let Some(mut ask) = covering {
            ask.queued = Some(queued.clone());
            tx.put(&ask_key, &ask)?;
        }
        held.drop_mark().queued = Some(queued);
        tx.put(key, &held)?;
        crate::send_holds::QueuedStop::put_resend_request(&mut tx, &signed)?;
        self.app
            .queue_signed_with(crate::protocol::BACKUP_NAME, signed, &tx.into_writes())
            .await?;
        let _ = self.send_message(&msg_id).await;
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

    /// When the request `mark` names as queued was delivered, as the outbox shows it, if
    /// that was before the mark expired and the message delivered is shown to be that
    /// request ([`Paykit::delivered_request`]); `None` for a request still queued or kept, one
    /// not sent, one delivered too late, or none. Nothing is sent or tried.
    ///
    /// # Errors
    ///
    /// A store failure; a corrupt store when the message delivered under its `msg_id` is
    /// another one.
    async fn delivered_in_time(&self, mark: &DropMark) -> Result<Option<u64>> {
        let Some(queued) = &mark.queued else {
            return Ok(None);
        };
        self.delivered_by(&mark.sender, mark.latest_ms, queued)
            .await
    }

    /// When `queued`, a request to `sender` for a mark whose latest time is `latest_ms`, was
    /// delivered, as the outbox shows it, if that was before such a mark expired and the
    /// message delivered is shown to be that request; `None` otherwise
    /// ([`Paykit::delivered_in_time`]). Nothing is sent or tried.
    ///
    /// # Errors
    ///
    /// A store failure; a corrupt store when the message delivered under its `msg_id` is
    /// another one.
    async fn delivered_by(
        &self,
        sender: &[u8; 32],
        latest_ms: u64,
        queued: &QueuedRequest,
    ) -> Result<Option<u64>> {
        let Some(Delivery::Accepted { accepted_at_ms }) = self.app.delivery(&queued.msg_id).await?
        else {
            return Ok(None);
        };
        if latest_ms.saturating_add(crate::KEPT_MESSAGE_MS) <= accepted_at_ms {
            return Ok(None);
        }
        self.delivered_request(sender, queued).await?;
        Ok(Some(accepted_at_ms))
    }

    /// Settle every notice owed on a block's mark the receive step dropped for the bound
    /// (`bl/`, [`LostOwed`]), each in its own commit: a request the outbox shows delivered
    /// before the mark's expiry tells nothing; one still queued, refused, kept in a batch, or
    /// not known to the outbox tells the window lost. Either way the owed notice goes in the
    /// commit that settles it. A failed read returns its error, leaving that notice and the
    /// ones after it owed for the next pass.
    ///
    /// After a restore the outbox holds the records the backup carried, under the same
    /// `msg_id`s, and none of the sent messages: a request still queued at the backup is
    /// queued again, and settles as it is delivered or not; one delivered before the backup
    /// whose record had left the outbox isn't known, and tells the window lost.
    ///
    /// Each notice is told at most once: the notice and the deletion of what owed it are one
    /// commit, and a crash before that commit leaves the owed notice as it was, settled anew
    /// at the next pass from the outbox's answer. A crash between the receive step's commit
    /// and this pass leaves it owed in the same way.
    ///
    /// # Errors
    ///
    /// A store failure; a corrupt store when the message delivered under an owed request's
    /// `msg_id` is another one.
    pub(crate) async fn settle_lost_owed(&mut self) -> Result<()> {
        for (rest, owed) in self.local_scan::<LostOwed>(key::LOST_OWED).await? {
            let queued = QueuedRequest {
                msg_id: owed.msg_id.clone(),
                hash: owed.hash,
            };
            let delivered = self
                .delivered_by(&owed.sender, owed.latest_ms, &queued)
                .await?;
            let mut tx = crate::state::Tx::default();
            if delivered.is_none() {
                tx.put(
                    &key::blocked_lost(&owed.sender, owed.earliest_ms),
                    &crate::notices::KeptNotice::blocked_lost(
                        owed.sender,
                        owed.earliest_ms,
                        owed.latest_ms,
                        owed.raised_ms,
                    ),
                )?;
            }
            tx.delete(&format!("{}{rest}", key::LOST_OWED));
            self.local_commit(tx).await?;
        }
        Ok(())
    }

    /// Check that the message the outbox delivered to `sender` under `queued`'s `msg_id` is
    /// that `resend_request`: one to `sender`, of that kind and hash.
    ///
    /// # Errors
    ///
    /// A corrupt store when none is: the mark points to another message.
    pub(crate) async fn delivered_request(
        &self,
        sender: &[u8; 32],
        queued: &QueuedRequest,
    ) -> Result<()> {
        let sent = self.app.sent_to(sender, 0, u64::MAX).await?;
        if sent
            .iter()
            .find(|message| message.msg_id == queued.msg_id)
            .is_some_and(|message| {
                queued.matches_signed_request(&message.kind, message.plaintext.hash())
            })
        {
            Ok(())
        } else {
            Err(PaykitError::corrupt())
        }
    }
}

/// Tell the host, in `tx`, that `gone`'s window can no longer come back, when `gone` records
/// no ask delivered: a block's mark that expires or that the bound drops (module docs). At
/// expiry and when a widening is applied, the delivery of a request the mark still names as
/// queued is read first ([`Paykit::delivered_in_time`]); the receive step, which makes no
/// client reads, tells here only for a mark the bound drops there that names no request as
/// queued, and keeps the notice owed for one that names one ([`LostOwed`]).
pub(crate) fn tell_lost_if_unasked(
    tx: &mut crate::state::Tx,
    gone: &DropMark,
    raised: u64,
) -> Result<()> {
    if gone.asked_at_ms.is_none() {
        tx.put(
            &key::blocked_lost(&gone.sender, gone.earliest_ms),
            &crate::notices::KeptNotice::blocked_lost(
                gone.sender,
                gone.earliest_ms,
                gone.latest_ms,
                raised,
            ),
        )?;
    }
    Ok(())
}

/// Whether a widening's commit with its raise applied: a raise that can't be written, for
/// now or while the divergence hold stands, leaves the widening owed, its marks and their
/// asks unapplied, to be tried again at the next read of the lists (Paykit wire 5.6,
/// Raising: a widening whose raise isn't applied isn't applied either). It never fails the
/// read of the lists.
fn raised(commit: Result<()>) -> Result<bool> {
    match commit {
        Ok(()) => Ok(true),
        Err(error) if crate::floor::is_raise_unwritten(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current_mark() -> DropMark {
        count_dropped(None, &[], [1; 32], 7, true).put.unwrap()
    }

    fn current_key_wait() -> KeyWaitMark {
        let mark = count_dropped(None, &[], [1; 32], 7, false).put.unwrap();
        KeyWaitMark::with_epoch(mark, Vec::new(), 2)
    }

    fn without(value: impl Serialize, member: &str) -> serde_json::Value {
        let mut value = serde_json::to_value(value).unwrap();
        assert!(value.as_object_mut().unwrap().remove(member).is_some());
        value
    }

    #[test]
    fn test_mark_state_drop_keeps_null_and_omitted_members() {
        let mark = current_mark();
        let value = serde_json::to_value(&mark).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 6);
        assert!(value.as_object().unwrap().contains_key("asked_at_ms"));
        assert!(value.as_object().unwrap().contains_key("queued"));
        assert!(value["asked_at_ms"].is_null());
        assert!(value["queued"].is_null());
        assert_eq!(serde_json::from_value::<DropMark>(value).unwrap(), mark);
    }

    #[test]
    fn test_mark_state_drop_requires_asked_at() {
        assert!(
            serde_json::from_value::<DropMark>(without(current_mark(), "asked_at_ms")).is_err()
        );
    }

    #[test]
    fn test_mark_state_drop_requires_queued() {
        assert!(serde_json::from_value::<DropMark>(without(current_mark(), "queued")).is_err());
    }

    #[test]
    fn test_mark_state_drop_refuses_unknown_member() {
        let mut value = serde_json::to_value(current_mark()).unwrap();
        value["extra"] = serde_json::json!({"nested": [1]});
        assert!(serde_json::from_value::<DropMark>(value).is_err());
    }

    #[test]
    fn test_mark_state_drop_refuses_epochs() {
        let value = serde_json::to_value(current_key_wait()).unwrap();
        assert!(serde_json::from_value::<DropMark>(value).is_err());
    }

    #[test]
    fn test_mark_state_key_wait_keeps_flat_members() {
        let mark = current_key_wait();
        let value = serde_json::to_value(&mark).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 7);
        assert_eq!(value["epochs"], serde_json::json!([2]));
        assert!(value.get("mark").is_none());
        assert!(value.get("pending_forget").is_none());
        assert!(value["asked_at_ms"].is_null());
        assert!(value["queued"].is_null());
        assert_eq!(serde_json::from_value::<KeyWaitMark>(value).unwrap(), mark);
    }

    #[test]
    fn test_mark_state_key_wait_requires_asked_at() {
        assert!(
            serde_json::from_value::<KeyWaitMark>(without(current_key_wait(), "asked_at_ms"))
                .is_err()
        );
    }

    #[test]
    fn test_mark_state_key_wait_requires_queued() {
        assert!(
            serde_json::from_value::<KeyWaitMark>(without(current_key_wait(), "queued")).is_err()
        );
    }

    #[test]
    fn test_mark_state_key_wait_refuses_unknown_member() {
        let mut value = serde_json::to_value(current_key_wait()).unwrap();
        value["extra"] = serde_json::json!({"nested": [1]});
        assert!(serde_json::from_value::<KeyWaitMark>(value).is_err());
    }

    #[test]
    fn test_mark_state_key_wait_refuses_nested_mark() {
        let mut value = serde_json::to_value(current_key_wait()).unwrap();
        value["mark"] = serde_json::to_value(current_mark()).unwrap();
        assert!(serde_json::from_value::<KeyWaitMark>(value).is_err());
    }

    #[test]
    fn test_mark_state_refuses_duplicate_members() {
        let mark = serde_json::to_string(&current_mark()).unwrap();
        let mark = mark.replacen('{', "{\"earliest_ms\":7,", 1);
        assert!(serde_json::from_str::<DropMark>(&mark).is_err());
        let mark = serde_json::to_string(&current_key_wait()).unwrap();
        let same = mark.replacen('{', "{\"earliest_ms\":7,", 1);
        assert!(serde_json::from_str::<KeyWaitMark>(&same).is_err());
        let same = mark.replacen('{', "{\"epochs\":[2],", 1);
        assert!(serde_json::from_str::<KeyWaitMark>(&same).is_err());
    }

    fn queued_request() -> QueuedRequest {
        QueuedRequest {
            msg_id: "123e4567-e89b-42d3-a456-426614174000".into(),
            hash: [7; 32],
        }
    }

    #[test]
    fn test_mark_state_queued_request_round_trips() {
        let request = queued_request();
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serde_json::from_value::<QueuedRequest>(value).unwrap(),
            request
        );
    }

    #[test]
    fn test_mark_state_queued_request_refuses_unknown_member() {
        let mut value = serde_json::to_value(queued_request()).unwrap();
        value["extra"] = serde_json::json!({"nested": [1]});
        assert!(serde_json::from_value::<QueuedRequest>(value).is_err());
    }

    #[test]
    fn test_mark_state_queued_request_requires_identity_members() {
        for member in ["msg_id", "hash"] {
            assert!(
                serde_json::from_value::<QueuedRequest>(without(queued_request(), member)).is_err()
            );
        }
        let mut value = serde_json::to_value(queued_request()).unwrap();
        value["kept_batch"] = serde_json::json!([1, 2]);
        assert!(serde_json::from_value::<QueuedRequest>(value).is_err());
        let value = serde_json::to_string(&queued_request()).unwrap();
        let duplicate = value.replacen(
            '{',
            "{\"msg_id\":\"123e4567-e89b-42d3-a456-426614174000\",",
            1,
        );
        assert!(serde_json::from_str::<QueuedRequest>(&duplicate).is_err());
    }

    fn mark(sender: u8, earliest_ms: u64, latest_ms: u64) -> DropMark {
        DropMark {
            sender: [sender; 32],
            earliest_ms,
            latest_ms,
            due: false,
            asked_at_ms: Some(3),
            queued: None,
            pending_forget: Vec::new(),
            speculative: false,
            late_told: false,
        }
    }

    /// `n` block marks of senders `100..`, unasked, with latest times `latest + i`.
    fn block_marks(n: usize, latest: u64, speculative: bool) -> Vec<DropMark> {
        (0..n)
            .map(|i| DropMark {
                asked_at_ms: None,
                due: true,
                speculative,
                ..mark(u8::try_from(i + 100).unwrap(), latest, latest + i as u64)
            })
            .collect()
    }

    #[test]
    fn test_count_blocked_asked_window_starts_anew() {
        let mut asked = mark(1, 10, 20);
        asked.queued = Some(QueuedRequest {
            msg_id: "m".into(),
            hash: [0; 32],
        });
        let BlockedCount::Put {
            mark: next,
            evicted,
        } = count_blocked(Some(asked), &[], [1; 32], (50, 50), true)
        else {
            panic!("counted");
        };
        assert_eq!((next.earliest_ms, next.latest_ms), (50, 50));
        assert_eq!((next.asked_at_ms, next.queued, evicted), (None, None, None));
        // Unasked, the window widens, and a request in flight is forgotten.
        let mut unasked = mark(1, 10, 20);
        unasked.asked_at_ms = None;
        unasked.queued = Some(QueuedRequest {
            msg_id: "m".into(),
            hash: [0; 32],
        });
        let BlockedCount::Put { mark: next, .. } =
            count_blocked(Some(unasked), &[], [1; 32], (5, 30), true)
        else {
            panic!("counted");
        };
        assert_eq!(
            (next.earliest_ms, next.latest_ms, next.queued),
            (5, 30, None)
        );
    }

    #[test]
    fn test_count_blocked_new_window_never_lowers_the_latest_time() {
        // An older `missed` window applied over an asked mark starts a new window, but its
        // latest time stays: a request of the old window may still come back.
        let asked = mark(1, 50, 100);
        let BlockedCount::Put { mark: next, .. } =
            count_blocked(Some(asked), &[], [1; 32], (10, 20), false)
        else {
            panic!("counted");
        };
        assert_eq!((next.earliest_ms, next.latest_ms), (10, 100));
        assert_eq!(next.asked_at_ms, None);
    }

    #[test]
    fn test_count_blocked_speculative_marks_go_before_real_ones() {
        // Past the bound, a real message evicts the speculative mark, though it is newer
        // than every real one.
        let mut others = block_marks(MAX_DROPPED_MARKS - 1, 10, false);
        others.extend(block_marks(1, 1_000, true).into_iter().map(|mut m| {
            m.sender = [250; 32];
            m
        }));
        let BlockedCount::Put { evicted, .. } =
            count_blocked(None, &others, [1; 32], (2_000, 2_000), true)
        else {
            panic!("counted");
        };
        assert_eq!(evicted.map(|m| m.sender), Some([250; 32]));
        // A widening past the bound of real marks drops its own speculative mark.
        let real = block_marks(MAX_DROPPED_MARKS, 10, false);
        assert_eq!(
            count_blocked(None, &real, [1; 32], (5, 3_000), false),
            BlockedCount::Dropped
        );
    }

    #[test]
    fn test_count_blocked_mark_with_pairs_to_forget_never_goes() {
        let mut others = block_marks(MAX_DROPPED_MARKS, 10, false);
        for mark in &mut others {
            mark.pending_forget = vec!["m".into()];
        }
        assert_eq!(
            count_blocked(None, &others, [1; 32], (2_000, 2_000), true),
            BlockedCount::Held
        );
        // One free of them goes, however new.
        others[MAX_DROPPED_MARKS - 1].pending_forget.clear();
        let BlockedCount::Put { evicted, .. } =
            count_blocked(None, &others, [1; 32], (2_000, 2_000), true)
        else {
            panic!("counted");
        };
        assert_eq!(
            evicted.map(|m| m.sender),
            Some(others[MAX_DROPPED_MARKS - 1].sender)
        );
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
