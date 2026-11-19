//! Delivery evidence for requests to recover a sender's dropped replies (Paykit spec 7.2).
//! The drop mark can expire or be evicted independently of an uncertain delivery.

use std::collections::BTreeMap;

use paykit_lib::bodies::{Body, ResendRequest};
use pubky_switch_client::app::{AppEvent, Delivery, LocalWrite, RestoreContext, SignedMessage};
use pubky_switch_core::enc::serde_b64;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Result};
use crate::marks::{DropMark, MAX_DROPPED_MARKS};
use crate::records::KeptMessage;
use crate::state::{self, key, Binder, Tx};
use crate::Paykit;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Outcome {
    Pending,
    Restored,
    Unknown,
    Accepted {
        observed_at_ms: u64,
        accepted_at_ms: u64,
    },
    Undelivered,
}

impl Outcome {
    /// A former installation can know that its own attempt was never sent while
    /// another copy of the same original reports potential delivery.
    fn with(self, other: Self) -> Self {
        match (self, other) {
            (at @ Self::Accepted { .. }, _) | (_, at @ Self::Accepted { .. }) => at,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::Undelivered, _) | (_, Self::Undelivered) => Self::Undelivered,
            (Self::Restored, _) | (_, Self::Restored) => Self::Restored,
            (Self::Pending, Self::Pending) => Self::Pending,
        }
    }

    fn settled(self) -> bool {
        matches!(self, Self::Accepted { .. } | Self::Undelivered)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Original {
    id: String,
    #[serde(with = "serde_b64")]
    hash: [u8; 32],
    message: KeptMessage,
    from_ms: u64,
    latest_ms: u64,
    generation: u64,
    outcome: Outcome,
}

impl Original {
    fn covers_window(&self, from: u64, until: u64) -> bool {
        self.from_ms <= from && until <= self.latest_ms
    }

    fn covers(&self, mark: &DropMark) -> bool {
        match &mark.queued {
            Some(q) => {
                q.msg_id == self.id
                    && q.hash == self.hash
                    && self.from_ms <= mark.latest_ms
                    && mark.earliest_ms <= self.latest_ms
            }
            None => self.covers_window(mark.earliest_ms, mark.latest_ms),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetiredWindow {
    from_ms: u64,
    until_ms: u64,
    raised_ms: u64,
}

/// The open-ended asks identified for a sender, with the earliest unresolved window for
/// which a selected backup could not identify any original.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplyAsks {
    #[serde(deserialize_with = "Option::deserialize")]
    unidentified_from: Option<u64>,
    originals: Vec<Original>,
    retired: Vec<RetiredWindow>,
}

impl ReplyAsks {
    pub(crate) fn check(&self, sender: &[u8; 32], binder: &Binder<'_>) -> Result<()> {
        let mut ids = std::collections::BTreeSet::new();
        if self.originals.len() > MAX_DROPPED_MARKS {
            return Err(PaykitError::corrupt());
        }
        let mut windows = std::collections::BTreeSet::new();
        if self
            .retired
            .iter()
            .any(|w| w.from_ms > w.until_ms || !windows.insert(w.from_ms))
        {
            return Err(PaykitError::corrupt());
        }
        for original in &self.originals {
            let stored = original.message.stored(sender)?;
            let Body::ResendRequest(body) = stored.body() else {
                return Err(PaykitError::corrupt());
            };
            if original.generation == 0
                || stored.sender() != *binder.identity()
                || Some(stored.cert().app_id()) != binder.app_id()
                || stored.message().msg_id != original.id
                || stored.hash() != original.hash
                || original.from_ms > original.latest_ms
                || body.accepted_from != Some(original.from_ms)
                || body.accepted_until.is_some()
                || !body.msg_ids.is_empty()
                || !body.request_hashes.is_empty()
                || !ids.insert(&original.id)
            {
                return Err(PaykitError::corrupt());
            }
        }
        Ok(())
    }

    /// A completed ask covers only the dropped times it was prepared for, and only
    /// when the sender accepted it while that window could still be recovered.
    fn delivered_in_time(&self, from: u64, until: u64) -> bool {
        self.originals.iter().any(|o| {
            o.covers_window(from, until)
                && matches!(o.outcome, Outcome::Accepted {accepted_at_ms, ..}
                    if accepted_at_ms < until.saturating_add(crate::KEPT_MESSAGE_MS))
        })
    }

    pub(crate) fn lost_notice(
        &self,
        sender: &[u8; 32],
        from: u64,
        until: u64,
        raised: u64,
    ) -> Option<(String, crate::notices::KeptNotice)> {
        if self.delivered_in_time(from, until) {
            return None;
        }
        Some((
            key::dropped_lost(sender, "pd", from),
            crate::notices::KeptNotice::dropped_lost(*sender, from, until, raised),
        ))
    }

    /// Only a closed time selection wholly before the held window is disjoint. An ID or
    /// request hash can select a message in the window independently of any time fields.
    pub(crate) fn permits(&self, body: &ResendRequest, pending: Option<(&str, [u8; 32])>) -> bool {
        let overlaps = |from| {
            !body.msg_ids.is_empty()
                || !body.request_hashes.is_empty()
                || body.accepted_until.is_none_or(|until| until >= from)
        };
        !self.unidentified_from.is_some_and(overlaps)
            && self.originals.iter().all(|original| {
                original.outcome.settled()
                    || !overlaps(original.from_ms)
                    || (original.outcome == Outcome::Pending
                        && pending == Some((&original.id, original.hash)))
            })
    }

    fn hold_unidentified(&mut self, from: u64) {
        self.unidentified_from = Some(self.unidentified_from.map_or(from, |prior| prior.min(from)));
    }

    fn retire(&mut self, window: RetiredWindow) {
        if let Some(before) = self
            .retired
            .iter_mut()
            .find(|older| older.from_ms == window.from_ms)
        {
            before.raised_ms = before.raised_ms.min(window.raised_ms);
            before.until_ms = before.until_ms.max(window.until_ms);
        } else {
            self.retired.push(window);
        }
    }

    fn import(&mut self, other: Self) -> Result<()> {
        if let Some(from) = other.unidentified_from {
            self.hold_unidentified(from);
        }
        for window in other.retired {
            self.retire(window);
        }
        for mut incoming in other.originals {
            if incoming.outcome == Outcome::Pending {
                incoming.outcome = Outcome::Restored;
            }
            if let Some(existing) = self.originals.iter_mut().find(|v| v.id == incoming.id) {
                if existing.hash != incoming.hash
                    || existing.latest_ms != incoming.latest_ms
                    || existing.generation != incoming.generation
                {
                    return Err(PaykitError::corrupt());
                }
                existing.outcome = existing.outcome.with(incoming.outcome);
            } else {
                self.originals.push(incoming);
            }
        }
        Ok(())
    }
}

/// Retire the recovery obligation in the intake commit. If the native outbox could have
/// accepted an identified request, the next SDK pass checks it before telling the host.
pub(crate) fn retire_in_receive(
    cx: &mut pubky_switch_client::app::ReceiveContext<'_>,
    sender: &[u8; 32],
    from_ms: u64,
    until_ms: u64,
) -> std::result::Result<(), crate::receive::Fail> {
    let key = key::reply_asks(sender);
    let mut held = state::cx_get::<ReplyAsks>(cx, &key)?.unwrap_or_default();
    let raised_ms = cx.highest_service_time().unwrap_or(0);
    if held.originals.iter().any(|o| !o.outcome.settled()) {
        held.retire(RetiredWindow {
            from_ms,
            until_ms,
            raised_ms,
        });
        crate::receive::keep_local(cx, crate::receive::local(&key, &held)?);
    } else if let Some((key, notice)) = held.lost_notice(sender, from_ms, until_ms, raised_ms) {
        crate::receive::keep_local(cx, crate::receive::local(&key, &notice)?);
    }
    Ok(())
}

/// Combine the selected history with evidence from this installation before replacing it.
pub(crate) fn restore(
    restoring: &RestoreContext<'_>,
    binder: &Binder<'_>,
    state: &[(Vec<u8>, Vec<u8>)],
) -> std::result::Result<Vec<LocalWrite>, String> {
    let previous = restoring
        .previous_local_scan(key::REPLY_ASKS.as_bytes())
        .map_err(|_| "the installation's recovery requests cannot be read")?;
    let mut requests = BTreeMap::<[u8; 32], ReplyAsks>::new();
    for (k, v) in previous
        .iter()
        .chain(state)
        .filter(|(k, _)| k.starts_with(key::REPLY_ASKS.as_bytes()))
    {
        let Some(state::StateKey::ReplyAsks { sender }) = state::StateKey::parse(k) else {
            return Err("invalid recovery request key".into());
        };
        let source = state::restored::<ReplyAsks>(k, v, binder)
            .map_err(|_| "invalid recovery request evidence")?;
        requests
            .entry(sender)
            .or_default()
            .import(source)
            .map_err(|_| "conflicting recovery request evidence")?;
    }
    for (k, v) in state
        .iter()
        .filter(|(k, _)| k.starts_with(key::DROPPED_REPLIES.as_bytes()))
    {
        let mark =
            state::restored::<DropMark>(k, v, binder).map_err(|_| "invalid dropped reply mark")?;
        let held = requests.entry(mark.sender).or_default();
        let identified = held.originals.iter().any(|original| original.covers(&mark));
        if !identified {
            held.hold_unidentified(mark.earliest_ms);
        }
    }
    requests
        .into_iter()
        .map(|(sender, held)| {
            held.check(&sender, binder)
                .map_err(|_| "invalid merged recovery request evidence")?;
            state::local_put(&key::reply_asks(&sender), &held)
                .map_err(|_| "recovery request evidence cannot be written".into())
        })
        .collect()
}

impl Paykit {
    pub(crate) async fn recovery_request_permitted(
        &self,
        to: &[u8; 32],
        body: &ResendRequest,
        original: Option<(&str, [u8; 32])>,
    ) -> Result<bool> {
        let held = self.local_get::<ReplyAsks>(&key::reply_asks(to)).await?;
        Ok(held.is_none_or(|held| held.permits(body, original)))
    }

    /// Reserve the identified first attempt in the same commit as the mark's pointer.
    pub(crate) async fn reserve_reply_ask(
        &self,
        tx: &mut Tx,
        mark: &DropMark,
        signed: &SignedMessage,
    ) -> Result<bool> {
        let k = key::reply_asks(&mark.sender);
        let mut held = self.local_get::<ReplyAsks>(&k).await?.unwrap_or_default();
        let message = self.own_copy(signed)?;
        let Body::ResendRequest(body) = message.body() else {
            return Err(PaykitError::corrupt());
        };
        if !held.permits(body, None) {
            return Ok(false);
        }
        // Retain the last positive coverage if another ask becomes uncertain.
        let covered = std::mem::take(&mut held.originals).into_iter().rfind(|o| {
            matches!(o.outcome, Outcome::Accepted { .. })
                && o.covers_window(mark.earliest_ms, mark.latest_ms)
        });
        held.originals.extend(covered);
        held.originals.push(Original {
            id: signed.msg_id().into(),
            hash: signed.hash(),
            message: KeptMessage::of(&message),
            from_ms: mark.earliest_ms,
            latest_ms: mark.latest_ms,
            generation: self.app.own_identity().generation(),
            outcome: Outcome::Pending,
        });
        tx.put(&k, &held)?;
        Ok(true)
    }

    /// Bring the mark and its delivery evidence up to date together. An absent sent row
    /// or a final failure of a later try proves nothing about a previous try.
    pub(crate) async fn reconcile_reply_asks(&mut self) -> Result<()> {
        for (peer, mut held) in self.local_scan::<ReplyAsks>(key::REPLY_ASKS).await? {
            if held.originals.is_empty() && held.retired.is_empty() {
                continue;
            }
            let sender = pubky_switch_core::enc::unb64_array::<32>(&peer)
                .map_err(|_| PaykitError::corrupt())?;
            let k = key::reply_asks(&sender);
            let mark_key = key::dropped_replies(&sender);
            let mut mark = self.local_get::<DropMark>(&mark_key).await?;
            let previous_mark = mark.clone();
            let mut changed = false;
            let mut tx = Tx::default();
            for original in &mut held.originals {
                if !original.outcome.settled() {
                    match self.app.delivery(&original.id).await? {
                        Some(Delivery::Accepted { accepted_at_ms }) => {
                            let expected = original.message.stored(&sender)?;
                            let sent = self.app.sent_to(&sender, 0, u64::MAX).await?;
                            if !sent.iter().any(|s| {
                                s.to == sender
                                    && s.msg_id == original.id
                                    && s.kind == expected.message().kind
                                    && &s.plaintext == expected.plaintext()
                                    && s.accepted_at_ms == Some(accepted_at_ms)
                            }) {
                                return Err(PaykitError::corrupt());
                            }
                            original.outcome = Outcome::Accepted {
                                observed_at_ms: self.now(),
                                accepted_at_ms,
                            };
                            changed = true;
                        }
                        Some(Delivery::Failed { .. })
                            if original.outcome != Outcome::Unknown
                                && original.generation == self.app.own_identity().generation() =>
                        {
                            use pubky_switch_client::app::{KeptBatchDrop, KeptBatchStatus};
                            if let Some(batch) = mark
                                .as_ref()
                                .and_then(|m| m.queued.as_ref())
                                .filter(|q| q.msg_id == original.id && q.hash == original.hash)
                                .and_then(|q| q.kept_batch)
                            {
                                if matches!(
                                    self.app.kept_batch(batch).await?,
                                    Some(KeptBatchStatus::Dropped {
                                        reason: KeptBatchDrop::Unsent
                                            | KeptBatchDrop::Refused { .. },
                                        ..
                                    })
                                ) {
                                    original.outcome = Outcome::Undelivered;
                                    changed = true;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(mark) = mark.as_mut() {
                    if original.covers(mark) && original.outcome.settled() {
                        if let Outcome::Accepted {
                            observed_at_ms: at, ..
                        } = original.outcome
                        {
                            mark.asked_at_ms = Some(mark.asked_at_ms.map_or(at, |old| old.max(at)));
                        }
                        mark.queued = None;
                    }
                }
            }
            for window in std::mem::take(&mut held.retired) {
                if let Some((key, notice)) =
                    held.lost_notice(&sender, window.from_ms, window.until_ms, window.raised_ms)
                {
                    tx.put(&key, &notice)?;
                }
                changed = true;
            }
            if let Some(mark) = &mark {
                if Some(mark) != previous_mark.as_ref() {
                    tx.put(&mark_key, mark)?;
                }
            } else {
                let before = held.originals.len();
                held.originals.retain(|v| !v.outcome.settled());
                changed |= held.originals.len() != before;
            }
            if changed {
                if held.originals.is_empty() && held.unidentified_from.is_none() {
                    tx.delete(&k);
                } else {
                    tx.put(&k, &held)?;
                }
            }
            self.local_commit(tx).await?;
        }
        Ok(())
    }

    /// Only the owning Paykit request may take a first-delivery duty, with its outcome
    /// in the very commit used to acknowledge it. Re-send duties remain with their owner.
    pub(crate) async fn take_reply_ask_duty(&self, tx: &mut Tx, event: &AppEvent) -> Result<bool> {
        let (id, outcome) = match event {
            AppEvent::MessageUndelivered {
                msg_id,
                resend: false,
                ..
            } => (msg_id, Outcome::Undelivered),
            AppEvent::DeliveryUnknown { msg_id, .. } => (msg_id, Outcome::Unknown),
            _ => return Ok(false),
        };
        for (peer, mut held) in self.local_scan::<ReplyAsks>(key::REPLY_ASKS).await? {
            if let Some(original) = held.originals.iter_mut().find(|v| &v.id == id) {
                let observed = original.outcome.with(outcome);
                if original.outcome != observed {
                    original.outcome = observed;
                    tx.put(&format!("{}{peer}", key::REPLY_ASKS), &held)?;
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The floor write was refused before Switch enqueued any bytes.
    pub(crate) async fn reply_ask_not_queued(&mut self, sender: &[u8; 32], id: &str) -> Result<()> {
        let key = key::reply_asks(sender);
        let mut held = self
            .local_get::<ReplyAsks>(&key)
            .await?
            .ok_or_else(PaykitError::corrupt)?;
        let original = held
            .originals
            .iter_mut()
            .find(|o| o.id == id)
            .ok_or_else(PaykitError::corrupt)?;
        original.outcome = Outcome::Undelivered;
        self.local_put(&key, &held).await
    }
}

#[cfg(test)]
mod tests {
    use super::Outcome;

    #[test]
    fn test_restore_composes_observations_of_the_same_signed_original() {
        let accepted = Outcome::Accepted {
            observed_at_ms: 17,
            accepted_at_ms: 12,
        };
        for (left, right, expected) in [
            (
                Outcome::Restored,
                Outcome::Undelivered,
                Outcome::Undelivered,
            ),
            (Outcome::Unknown, Outcome::Undelivered, Outcome::Unknown),
            (Outcome::Restored, Outcome::Unknown, Outcome::Unknown),
            (Outcome::Unknown, accepted, accepted),
            (Outcome::Undelivered, accepted, accepted),
        ] {
            assert_eq!(left.with(right), expected);
            assert_eq!(right.with(left), expected);
        }
    }
}
