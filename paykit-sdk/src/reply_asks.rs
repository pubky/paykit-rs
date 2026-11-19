//! Delivery evidence for requests to recover a sender's dropped replies (Paykit spec 7.2).
//! The drop mark can expire or be evicted independently of an uncertain delivery.

use pubky_switch_core::enc::b64;
use std::collections::{BTreeMap, BTreeSet};

use paykit_lib::bodies::{Body, ResendRequest};
use pubky_switch_client::app::{
    AppEvent, Delivery, KeptBatchDrop, KeptBatchStatus, LocalWrite, RestoreContext,
};
use pubky_switch_core::enc::serde_b64;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Result};
use crate::marks::{DropMark, QueuedRequest, MAX_DROPPED_MARKS};
use crate::notices::{KeptNotice, NoticeKind};
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
    with_floor: bool,
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
    fn is_empty(&self) -> bool {
        self.unidentified_from.is_none() && self.originals.is_empty() && self.retired.is_empty()
    }

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
                    || existing.with_floor != incoming.with_floor
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

/// Pending checks and unacknowledged loss notices share one budget. Converting a
/// pending check to its notice in one commit uses the slot it already occupies.
fn loss_windows_fit<'a>(
    retired: impl Iterator<Item = (String, &'a ReplyAsks)>,
    notices: impl Iterator<Item = &'a KeptNotice>,
    new: Option<(&[u8; 32], u64)>,
) -> bool {
    let pending = retired.flat_map(|(sender, asks)| {
        asks.retired
            .iter()
            .map(move |w| (sender.clone(), w.from_ms))
    });
    let shown = notices
        .filter(|n| n.kind == NoticeKind::DroppedMessagesLost)
        .filter_map(|n| Some((n.counterparty.clone()?, n.lost?.0)));
    let new = new.map(|(sender, from)| (b64(sender), from));
    let mut windows = BTreeSet::new();
    pending.chain(shown).chain(new).all(|window| {
        windows.insert(window);
        windows.len() <= crate::marks::MAX_LOST_OWED
    })
}

/// Independent observations at the same notice key can enlarge the lost window.
fn joined_loss(mut notice: KeptNotice, previous: Option<&KeptNotice>) -> KeptNotice {
    if let Some(previous) = previous {
        notice.raised_at_ms = notice.raised_at_ms.min(previous.raised_at_ms);
        notice.lost = notice.lost.max(previous.lost);
    }
    notice
}

/// Retire the recovery obligation in the intake commit. If the native outbox could have
/// accepted an identified request, the next SDK pass checks it before telling the host.
pub(crate) fn retire_in_receive(
    cx: &mut pubky_switch_client::app::ReceiveContext<'_>,
    retired: &[([u8; 32], u64, u64)],
) -> std::result::Result<(), crate::receive::Fail> {
    if retired.is_empty() {
        return Ok(());
    }
    let writes = retired_writes(
        state::cx_scan::<ReplyAsks>(cx, key::REPLY_ASKS)?
            .into_iter()
            .collect(),
        state::cx_scan::<KeptNotice>(cx, key::NOTICES)?
            .into_iter()
            .collect(),
        retired,
        cx.highest_service_time().unwrap_or(0),
    )?;
    for write in writes {
        crate::receive::keep_local(cx, write);
    }
    Ok(())
}

/// Plan all losses of this input before staging any of their writes. A stop can
/// displace more than one sender when previously protected entries become countable.
fn retired_writes(
    mut asks: BTreeMap<String, ReplyAsks>,
    mut notices: BTreeMap<String, KeptNotice>,
    retired: &[([u8; 32], u64, u64)],
    raised_ms: u64,
) -> std::result::Result<Vec<LocalWrite>, crate::receive::Fail> {
    let mut staged = BTreeMap::new();
    for &(sender, from_ms, until_ms) in retired {
        let ask_key = key::reply_asks(&sender);
        let held = asks.entry(ask_key.clone()).or_default();
        let Some((notice_key, notice)) = held.lost_notice(&sender, from_ms, until_ms, raised_ms)
        else {
            continue;
        };
        let previous = notices.get(&notice_key);
        if previous.is_some_and(|n| n.lost >= notice.lost) {
            continue;
        }
        if held.originals.iter().any(|o| !o.outcome.settled()) {
            held.retire(RetiredWindow {
                from_ms,
                until_ms,
                raised_ms,
            });
            let write = crate::receive::local(&ask_key, held)?;
            staged.insert(ask_key, write);
        } else {
            let loss = joined_loss(notice, previous);
            staged.insert(
                notice_key.clone(),
                crate::receive::local(&notice_key, &loss)?,
            );
            notices.insert(notice_key, loss);
        }
    }
    let pending = asks.iter().filter_map(|(key, ask)| {
        key.strip_prefix(key::REPLY_ASKS)
            .map(|sender| (sender.to_owned(), ask))
    });
    if !loss_windows_fit(pending, notices.values(), None) {
        return Err(crate::receive::Fail::AwaitAdmission);
    }
    Ok(staged.into_values().collect())
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
    let previous_markers = restoring
        .previous_local_scan(key::REPLY_FLOOR_BATCHES.as_bytes())
        .map_err(|_| "the original floor batches cannot be read")?;
    let floor_requests: BTreeMap<_, _> = requests
        .values()
        .flat_map(|r| &r.originals)
        .filter(|o| o.with_floor)
        .map(|o| (o.hash, o.outcome))
        .collect();
    let mut markers = BTreeMap::<[u8; 32], Option<[u8; 16]>>::new();
    for (k, value) in previous_markers
        .iter()
        .chain(state)
        .filter(|(k, _)| k.starts_with(key::REPLY_FLOOR_BATCHES.as_bytes()))
    {
        let Some(state::StateKey::ReplyFloorBatch { hash }) = state::StateKey::parse(k) else {
            return Err("invalid recovery request floor batch key".into());
        };
        let id = state::restored::<[u8; 16]>(k, value, binder)
            .map_err(|_| "invalid recovery request floor batch ID")?;
        if !floor_requests.contains_key(&hash) {
            return Err("the floor batch has no signed recovery request".into());
        }
        markers
            .entry(hash)
            .and_modify(|held| {
                if *held != Some(id) {
                    *held = None;
                }
            })
            .or_insert(Some(id));
    }
    let retained = restoring
        .previous_local_scan(key::NOTICES.as_bytes())
        .map_err(|_| "the installation's loss notices cannot be read")?;
    let mut losses = BTreeMap::new();
    for (k, v) in retained.iter().chain(state) {
        if matches!(
            state::StateKey::parse(k),
            Some(state::StateKey::DroppedLost { .. })
        ) {
            let notice = state::restored::<KeptNotice>(k, v, binder)
                .map_err(|_| "invalid dropped-message loss notice")?;
            let key = std::str::from_utf8(k).map_err(|_| "invalid loss notice key")?;
            let joined = joined_loss(notice, losses.get(key));
            losses.insert(key, joined);
        }
    }
    // An existing notice settles just the part already visible to the host. Its
    // acknowledgment must not recreate it from an older pending check.
    for (sender, asks) in &mut requests {
        asks.retired.retain(|w| {
            let key = key::dropped_lost(sender, "pd", w.from_ms);
            !losses
                .get(key.as_str())
                .is_some_and(|n| n.lost >= Some((w.from_ms, w.until_ms)))
        });
    }
    if !loss_windows_fit(
        requests.iter().map(|(sender, a)| (b64(sender), a)),
        losses.values(),
        None,
    ) {
        return Err("too many retired message windows and loss notices".into());
    }
    let mut writes = Vec::new();
    for (key, notice) in losses {
        writes.push(state::local_put(key, &notice).map_err(|_| "invalid loss notice")?);
    }
    for (hash, outcome) in floor_requests {
        if let Some(id) = markers
            .remove(&hash)
            .flatten()
            .filter(|_| !outcome.settled())
        {
            writes.push(LocalWrite::Put {
                key: key::reply_floor_batch(&hash).into_bytes(),
                value: id.to_vec(),
            });
        }
    }
    for (sender, held) in requests {
        held.check(&sender, binder)
            .map_err(|_| "invalid merged recovery request evidence")?;
        if held.is_empty() {
            continue;
        }
        writes.push(
            state::local_put(&key::reply_asks(&sender), &held)
                .map_err(|_| "recovery request evidence cannot be written")?,
        );
    }
    Ok(writes)
}

/// A queued re-send request may proceed when the sender has no overlapping uncertainty,
/// or when the mark still identifies this exact pending original (Paykit spec 7.2).
pub(crate) fn may_deliver_request(
    delivery: &pubky_switch_client::app::DeliveryView<'_>,
) -> Result<bool> {
    let message = delivery.message();
    let Some(asks) = state::delivery_get::<ReplyAsks>(delivery, &key::reply_asks(&message.to))?
    else {
        return Ok(true);
    };
    let body = ResendRequest::from_body(&message.body).map_err(|_| PaykitError::corrupt())?;
    if asks.permits(&body, None) {
        return Ok(true);
    }
    let mark = state::delivery_get::<DropMark>(delivery, &key::dropped_replies(&message.to))?;
    Ok(mark.and_then(|m| m.queued).is_some_and(|q| {
        q.msg_id == message.msg_id
            && q.hash == delivery.hash()
            && asks.permits(&body, Some((&q.msg_id, q.hash)))
    }))
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

    /// Enlist the exact signed request with its mark, optionally behind a floor raise.
    pub(crate) async fn queue_reply_ask(
        &mut self,
        mut mark: DropMark,
        selection: crate::ResendSelection,
    ) -> Result<u64> {
        let batch = match self.resend_floor_batch(&mark.sender).await {
            Ok(batch) => batch,
            Err(error) if crate::floor::floor_write_waits(&error) => return Ok(0),
            Err(error) => return Err(error),
        };
        let k = key::reply_asks(&mark.sender);
        let mut held = self.local_get::<ReplyAsks>(&k).await?.unwrap_or_default();
        if !held.permits(&selection.request(), None) {
            return Ok(0);
        }
        let signed = self.resend_request_message(mark.sender, selection)?;
        let id = signed.msg_id().to_owned();
        let hash = signed.hash();
        // Retain the last positive coverage if another ask becomes uncertain.
        let covered = std::mem::take(&mut held.originals).into_iter().rfind(|o| {
            matches!(o.outcome, Outcome::Accepted { .. })
                && o.covers_window(mark.earliest_ms, mark.latest_ms)
        });
        held.originals.extend(covered);
        held.originals.push(Original {
            id: id.clone(),
            hash,
            message: KeptMessage::signed(self.identity(), self.app.cert().signed(), &signed),
            from_ms: mark.earliest_ms,
            latest_ms: mark.latest_ms,
            generation: self.app.own_identity().generation(),
            with_floor: batch.is_some(),
            outcome: Outcome::Pending,
        });
        mark.queued = Some(QueuedRequest {
            msg_id: id.clone(),
            hash,
            kept_batch: None,
        });
        let mut tx = Tx::default();
        tx.put(&key::dropped_replies(&mark.sender), &mark)?;
        tx.put(&k, &held)?;
        if let Some(batch) = batch {
            // Only the first keep can enlist the floor write. Its marker survives until
            // this original is settled, including a lost local response to that keep.
            self.local_commit(tx).await?;
            let batch = batch
                .keep_id_at(key::reply_floor_batch(&hash).into_bytes())
                .queue(signed);
            if let Err(error) = self
                .submit_floor(batch)
                .await
                .map_err(crate::floor::floor_write_error)
            {
                if error.kept_batch().is_some() {
                    return Ok(1);
                }
                if crate::floor::floor_write_waits(&error)
                    || error.code() == Some(pubky_switch_core::http::ErrorCode::Conflict)
                {
                    return Ok(0);
                }
                return Err(error);
            }
        } else {
            self.app
                .queue_signed_with(crate::protocol::BACKUP_NAME, signed, &tx.into_writes())
                .await?;
        }
        if matches!(
            self.send_message(&id).await,
            Ok(Some(Delivery::Accepted { .. }))
        ) {
            self.reconcile_reply_asks().await?;
        }
        Ok(1)
    }

    /// A live mark remains owed when its expiry would create a loss beyond the bound.
    pub(crate) async fn record_expired_reply_loss(
        &self,
        tx: &mut Tx,
        sender: &[u8; 32],
        from: u64,
        until: u64,
    ) -> Result<bool> {
        let evidence = self
            .local_get::<ReplyAsks>(&key::reply_asks(sender))
            .await?
            .unwrap_or_default();
        let Some((key, loss)) =
            evidence.lost_notice(sender, from, until, self.service_time().unwrap_or(0))
        else {
            return Ok(true);
        };
        let previous = self.kept_notice(&key).await?;
        if previous.is_none() {
            let asks = self.local_scan::<ReplyAsks>(key::REPLY_ASKS).await?;
            let notices = self.local_scan::<KeptNotice>(key::NOTICES).await?;
            if !loss_windows_fit(
                asks.iter().map(|(sender, a)| (sender.clone(), a)),
                notices.iter().map(|(_, n)| n),
                Some((sender, from)),
            ) {
                return Ok(false);
            }
        }
        tx.put(&key, &joined_loss(loss, previous.as_ref()))?;
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
                        delivery @ (None | Some(Delivery::Failed { .. }))
                            if original.with_floor
                                && original.outcome != Outcome::Unknown
                                && original.generation == self.app.own_identity().generation() =>
                        {
                            let marker = key::reply_floor_batch(&original.hash);
                            let unsent = match self.local_get::<[u8; 16]>(&marker).await? {
                                Some(batch) => matches!(
                                    self.app.kept_batch(batch).await?,
                                    Some(KeptBatchStatus::Dropped {
                                        reason: KeptBatchDrop::Unsent
                                            | KeptBatchDrop::Refused { .. },
                                        ..
                                    })
                                ),
                                // This installation kept the intent but never enlisted its batch.
                                // A restore changes Pending to Restored even in place.
                                None => delivery.is_none() && original.outcome == Outcome::Pending,
                            };
                            if unsent {
                                original.outcome = Outcome::Undelivered;
                                changed = true;
                            }
                        }
                        _ => {}
                    }
                    if original.with_floor && original.outcome.settled() {
                        tx.delete(&key::reply_floor_batch(&original.hash));
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
                    let previous = self.kept_notice(&key).await?;
                    tx.put(&key, &joined_loss(notice, previous.as_ref()))?;
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
                if held.is_empty() {
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
                    if original.with_floor && observed.settled() {
                        tx.delete(&key::reply_floor_batch(&original.hash));
                    }
                    tx.put(&format!("{}{peer}", key::REPLY_ASKS), &held)?;
                }
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::{retired_writes, KeptNotice, Outcome};
    use crate::receive::Fail;
    use crate::state::key;
    use std::collections::BTreeMap;

    #[test]
    fn test_retiring_two_senders_admits_the_whole_effect_or_neither_loss() {
        let peer = [9; 32];
        let mut shown: BTreeMap<_, _> = (1..crate::marks::MAX_LOST_OWED as u64)
            .map(|from| {
                (
                    key::dropped_lost(&peer, "pd", from),
                    KeptNotice::dropped_lost(peer, from, from, from),
                )
            })
            .collect();
        let retired = [([1; 32], 50, 54), ([2; 32], 50, 50)];
        assert!(matches!(
            retired_writes(BTreeMap::new(), shown.clone(), &retired, 60),
            Err(Fail::AwaitAdmission)
        ));
        shown.remove(&key::dropped_lost(&peer, "pd", 1));
        let Ok(writes) = retired_writes(BTreeMap::new(), shown, &retired, 60) else {
            panic!("both newly retired senders fit");
        };
        assert_eq!(writes.len(), 2);
    }

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
