//! Marks of a sender's messages dropped before they were taken, and the re-sends they ask
//! for (Paykit spec 8.1, "Replies before their record" and "Stored entries"; Paykit spec 9).
//!
//! Two families, each with one mark per sender identity, the identity its certificate
//! names:
//!
//! - **Replies dropped for the bounds on kept replies** (`pd/<sender>`, `receive.rs`). Once
//!   the app holds the record of a request whose counterparty holds such a mark, it asks
//!   that sender to re-send what its inbox accepted from the mark's earliest time
//!   (`resend_request` with `accepted_from`). The client marked such a reply processed when
//!   it was kept, so at this pin a re-sent copy arrives as a duplicate and isn't taken: the
//!   client has no outcome yet for a message passed, not taken, that may come again.
//! - **Entries dropped unverified** (`ud/<sender>`, `stored.rs`). The app asks that sender
//!   at once to re-send what its inbox accepted from the mark's earliest time to its latest
//!   (`accepted_from` and `accepted_until`).
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

use pubky_switch_client::app::{AppEvent, Delivery};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::records::DAY_MS;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Result};
use crate::paykit::Paykit;
use crate::resend::ResendSelection;
use crate::state::{key, StateKey};

/// How many marks of each family the app keeps: past it, the mark whose latest time is
/// earliest goes, a tie going to the lower sender identity.
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
    /// Whether asking is due: for replies, once the app holds the record of one of the
    /// sender's requests; for entries dropped unverified, from the start.
    pub(crate) due: bool,
    /// When the outbox last delivered a request for this mark, on the device clock.
    pub(crate) asked_at_ms: Option<u64>,
    /// The `msg_id` of a request queued for this mark and not yet seen delivered.
    pub(crate) queued: Option<String>,
}

impl DropMark {
    /// Whether the mark has gone by `service`, the highest service time verified: its
    /// latest time plus [`crate::KEPT_MESSAGE_MS`] is at or before it.
    pub(crate) fn expired(&self, service: Option<u64>) -> bool {
        service.is_some_and(|t| self.latest_ms.saturating_add(crate::KEPT_MESSAGE_MS) <= t)
    }
}

/// The two families of marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MarkFamily {
    /// Replies dropped for the bounds on kept replies.
    Replies,
    /// Entries dropped unverified.
    Unverified,
}

impl MarkFamily {
    /// The family's key prefix.
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Replies => key::DROPPED_REPLIES,
            Self::Unverified => key::DROPPED_UNVERIFIED,
        }
    }

    /// The key of `sender`'s mark.
    pub(crate) fn key(self, sender: &[u8; 32]) -> String {
        match self {
            Self::Replies => key::dropped_replies(sender),
            Self::Unverified => key::dropped_unverified(sender),
        }
    }

    /// The sender a key of this family names; `None` for another key.
    pub(crate) fn sender_of(self, key: &str) -> Option<[u8; 32]> {
        match (self, StateKey::parse(key.as_bytes())?) {
            (Self::Replies, StateKey::DroppedReplies { sender })
            | (Self::Unverified, StateKey::DroppedUnverified { sender }) => Some(sender),
            _ => None,
        }
    }

    /// What a request for `mark` asks to re-send.
    fn selection(self, mark: &DropMark) -> ResendSelection {
        let selection = ResendSelection::default().with_accepted_from_ms(mark.earliest_ms);
        match self {
            Self::Replies => selection,
            Self::Unverified => selection.with_accepted_until_ms(mark.latest_ms),
        }
    }
}

/// Read a mark at `key` of `family`, bound to the sender its key names.
pub(crate) fn read_mark(family: MarkFamily, key: &str, value: &[u8]) -> Result<DropMark> {
    let mark = crate::state::decode::<DropMark>(value)?;
    if family.sender_of(key) != Some(mark.sender) || mark.earliest_ms > mark.latest_ms {
        return Err(PaykitError::corrupt());
    }
    Ok(mark)
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
        let held = match self
            .app
            .protocol_get(family.key(&sender).as_bytes())
            .await?
        {
            Some(value) => Some(read_mark(family, &family.key(&sender), &value)?),
            None => None,
        };
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

    /// Every mark of `family`, as (sender, latest time).
    async fn marks_of(&self, family: MarkFamily) -> Result<Vec<([u8; 32], u64)>> {
        let mut out = Vec::new();
        for (key, value) in self.app.protocol_scan(family.prefix().as_bytes()).await? {
            let key = String::from_utf8(key).map_err(|_| PaykitError::corrupt())?;
            let mark = read_mark(family, &key, &value)?;
            out.push((mark.sender, mark.latest_ms));
        }
        Ok(out)
    }

    /// Ask on the marks, at a receive (module docs): a reply mark becomes due once this read
    /// took the record of one of its sender's requests (`events`); a mark past its expiry
    /// goes; each due mark asks its sender when its cadence allows. Returns how many requests
    /// were queued.
    pub(crate) async fn ask_on_marks(&mut self, events: &[AppEvent]) -> Result<u64> {
        self.note_requests_held(events).await?;
        let service = self.service_time();
        let mut asked = 0u64;
        for family in [MarkFamily::Replies, MarkFamily::Unverified] {
            for (key, value) in self.app.protocol_scan(family.prefix().as_bytes()).await? {
                let key = String::from_utf8(key).map_err(|_| PaykitError::corrupt())?;
                let mark = read_mark(family, &key, &value)?;
                if mark.expired(service) {
                    self.local_delete(&key).await?;
                } else if mark.due {
                    asked = asked.saturating_add(self.ask_on_mark(family, &key, mark).await?);
                }
            }
        }
        Ok(asked)
    }

    /// Ask the sender of `mark`, at `key`, when its cadence allows (module docs). Returns
    /// how many requests were queued: 1 or 0.
    async fn ask_on_mark(
        &mut self,
        family: MarkFamily,
        key: &str,
        mut mark: DropMark,
    ) -> Result<u64> {
        if let Some(msg_id) = mark.queued.clone() {
            if matches!(
                self.app.delivery(&msg_id).await?,
                Some(Delivery::Queued { .. })
            ) {
                // Queued, it is tried again now; one that fails stays queued for the next
                // receive.
                let _ = self.app.flush_message(&msg_id).await;
            }
            match self.app.delivery(&msg_id).await? {
                Some(Delivery::Queued { .. }) => return Ok(0),
                Some(Delivery::Accepted { .. }) => mark.asked_at_ms = Some(self.now()),
                // Refused for good, or no longer kept: not sent, so asked anew below.
                _ => {}
            }
            mark.queued = None;
            self.local_put(key, &mark).await?;
        }
        let now = self.now();
        // A time ahead of the clock (restored, or the clock set back) is due: it never holds
        // the next request back for longer than a day.
        if mark
            .asked_at_ms
            .is_some_and(|at| at <= now && now - at < DAY_MS)
        {
            return Ok(0);
        }
        let signed = self.resend_request_message(mark.sender, family.selection(&mark))?;
        let msg_id = signed.msg_id().to_owned();
        // Kept before it is queued: a crash between the two finds the request not kept by
        // the outbox, and asks anew.
        mark.queued = Some(msg_id.clone());
        self.local_put(key, &mark).await?;
        self.app.queue_signed(signed).await?;
        let _ = self.app.flush_message(&msg_id).await;
        if matches!(
            self.app.delivery(&msg_id).await?,
            Some(Delivery::Accepted { .. })
        ) {
            mark.queued = None;
            mark.asked_at_ms = Some(self.now());
            self.local_put(key, &mark).await?;
        }
        Ok(1)
    }

    /// Make due the reply mark of each sender the record of one of whose requests this read
    /// took (`events`).
    async fn note_requests_held(&mut self, events: &[AppEvent]) -> Result<()> {
        let own = self.identity();
        let family = MarkFamily::Replies;
        for event in events {
            let AppEvent::RecordChanged { key: record } = event else {
                continue;
            };
            let Some(held) = self.app.shared_record(record).await? else {
                continue;
            };
            let (Some(_), Some(value)) = (
                paykit_lib::collections::Collection::PayeeActivity.policy_label(&held.policy),
                held.content.value(),
            ) else {
                continue;
            };
            let Ok(read) = paykit_lib::records::ActivityRecord::read(
                value,
                &own,
                paykit_lib::records::ActivitySide::Payee,
            ) else {
                continue;
            };
            let mark_key = family.key(&read.counterparty);
            let Some(bytes) = self.app.protocol_get(mark_key.as_bytes()).await? else {
                continue;
            };
            let mut mark = read_mark(family, &mark_key, &bytes)?;
            if !mark.due {
                mark.due = true;
                self.local_put(&mark_key, &mark).await?;
            }
        }
        Ok(())
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

    #[test]
    fn test_marks_read_refuses_another_sender_or_crossed_times() {
        let key = MarkFamily::Replies.key(&[1; 32]);
        let good = crate::state::encode(&mark(1, 5, 6)).unwrap();
        assert!(read_mark(MarkFamily::Replies, &key, &good).is_ok());
        assert!(read_mark(MarkFamily::Unverified, &key, &good).is_err());
        let other = crate::state::encode(&mark(2, 5, 6)).unwrap();
        assert!(read_mark(MarkFamily::Replies, &key, &other).is_err());
        let crossed = crate::state::encode(&mark(1, 7, 6)).unwrap();
        assert!(read_mark(MarkFamily::Replies, &key, &crossed).is_err());
    }
}
