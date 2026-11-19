//! A publisher's bounded replies to one chosen recipient (Paykit spec 8.4).

use super::{
    counter_unavailable, target, ListRecipient, ListReservation, ListSettings, COLLECTION, SCOPE,
};
use crate::error::{PaykitError, Result};
use crate::receive::{keep_local, local, Fail};
use crate::state::{self, key};
use crate::Paykit;
use paykit_lib::bodies::{PrivatePaymentListRequest, PRIVATE_LIST_MAX_AGE_MS};
use paykit_lib::records::StoredMessage;
use paykit_lib::{Kind, ListVersion};
use pubky_switch_client::app::{
    AppError, Delivery, ReceiveContext, ReceiveReport, RecordRead, Refusal, WontDeliver,
};
use serde::{Deserialize, Serialize};

const HOUR: u64 = 60 * 60 * 1000;
const DAY: u64 = 24 * HOUR;
const DAILY: usize = 4;
const OTHER: usize = 16;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Hint {
    version: Option<ListVersion>,
    id: String,
    hash: [u8; 32],
    seq: u64,
    at: u64,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RequestsAnswered {
    through: u64,
    named: Vec<ListVersion>,
    last: Option<Hint>,
    requested: bool,
}

impl RequestsAnswered {
    pub(super) fn check(&self) -> Result<()> {
        match &self.last {
            Some(last)
                if last.seq == self.through
                    && last.version.is_none_or(|v| self.named.contains(&v)) =>
            {
                pubky_switch_core::json::check_uuid_v4(&last.id)?;
            }
            None if self.through == 0 && self.named.is_empty() && !self.requested => {}
            _ => return Err(PaykitError::corrupt()),
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Offer {
    version: ListVersion,
    id: String,
    hash: [u8; 32],
    first: u64,
    expires: u64,
    answered: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LastAnswer {
    request: Hint,
    version: ListVersion,
    id: String,
    hash: [u8; 32],
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Repeat {
    id: String,
    hash: [u8; 32],
    /// Latest accepted repeat of this exact original before its newly owed repeat.
    previous: Option<u64>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Answers {
    hints: Vec<Hint>,
    offers: Vec<Offer>,
    other: Vec<(ListVersion, u64)>,
    daily: Vec<(ListVersion, u64)>,
    last: Option<LastAnswer>,
    last_repeat: Option<u64>,
    repeat: Option<Repeat>,
}

/// A verified permanent counter advanced beyond this installation's per-recipient
/// answer history. The missing publications could have spent the hourly/daily or
/// named-version bounds. The first covering service read dates that uncertainty.
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MissingAnswers {
    through: ListVersion,
    seen: u64,
    possible: usize,
}

impl MissingAnswers {
    pub(super) fn check(&self, current: ListVersion) -> Result<()> {
        if !(1..=DAILY).contains(&self.possible) || self.through >= current {
            return Err(PaykitError::corrupt());
        }
        Ok(())
    }

    pub(super) fn after(
        prior: Option<&ListReservation>,
        generation: u64,
        counter: u64,
        seen: u64,
        accepted: bool,
    ) -> Result<Option<Self>> {
        let inherited = prior.and_then(|issue| issue.missing);
        let known = prior
            .filter(|r| r.generation == generation)
            .map_or(0, |r| r.counter);
        let skipped = counter.saturating_sub(known);
        let uncertain = prior.is_some_and(|r| r.answer.requested && !accepted);
        if skipped == 0 && !uncertain {
            return Ok(inherited);
        }
        let through = if skipped > 0 {
            ListVersion::new(generation, counter)?
        } else {
            let issue = prior.ok_or_else(PaykitError::corrupt)?;
            ListVersion::new(issue.generation, issue.counter)?
        };
        let outstanding = inherited
            .filter(|gap| seen < gap.seen.saturating_add(DAY))
            .map_or(0, |gap| gap.possible);
        let count = skipped.min(DAILY as u64) as usize + usize::from(uncertain);
        Ok(Some(Self {
            through,
            seen,
            possible: DAILY.min(count + outstanding),
        }))
    }

    fn holds(self, hint: &Hint, now: u64) -> bool {
        hint.version.is_some_and(|named| {
            // An offer that already existed can remain tracked until 30 days after
            // expiry. Other arbitrary named pairs are remembered for 30 days.
            let retained = if named <= self.through {
                2 * PRIVATE_LIST_MAX_AGE_MS
            } else {
                PRIVATE_LIST_MAX_AGE_MS
            };
            now < self.seen.saturating_add(retained)
        })
    }
}

pub(super) enum Reply {
    Wait,
    Newer,
    Repeat,
}

impl Answers {
    pub(super) fn issues(&self) -> impl Iterator<Item = (ListVersion, &str, [u8; 32])> {
        self.offers
            .iter()
            .map(|offer| (offer.version, offer.id.as_str(), offer.hash))
            .chain(
                self.last
                    .iter()
                    .map(|answer| (answer.version, answer.id.as_str(), answer.hash)),
            )
    }

    pub(super) fn check(&self) -> Result<()> {
        if self.daily.len() > DAILY
            || self.other.len() > OTHER
            || self
                .offers
                .windows(2)
                .any(|pair| pair[0].version >= pair[1].version)
            || self.hints.len() > self.offers.len().saturating_add(OTHER + 2)
            || self
                .hints
                .iter()
                .map(|hint| hint.version)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.hints.len()
        {
            return Err(PaykitError::corrupt());
        }
        for offer in &self.offers {
            pubky_switch_core::json::check_uuid_v4(&offer.id)?;
        }
        for hint in &self.hints {
            pubky_switch_core::json::check_uuid_v4(&hint.id)?;
        }
        if let Some(repeat) = &self.repeat {
            pubky_switch_core::json::check_uuid_v4(&repeat.id)?;
        }
        if let Some(last) = &self.last {
            pubky_switch_core::json::check_uuid_v4(&last.request.id)?;
            pubky_switch_core::json::check_uuid_v4(&last.id)?;
        }
        Ok(())
    }

    /// Restoring two snapshots preserves distinct answers even when both inbox
    /// acceptances used the same millisecond. A restored request never chooses a recipient.
    pub(super) fn retain_history(&mut self, other: Self) {
        self.hints.clear();
        for offer in other.offers {
            if let Some(known) = self.offers.iter_mut().find(|o| o.version == offer.version) {
                known.expires = known.expires.max(offer.expires);
                known.answered = known.answered.max(offer.answered);
            } else {
                self.offers.push(offer);
            }
        }
        self.offers.sort_by_key(|offer| offer.version);
        for (version, at) in other.other {
            self.remember(version, at);
        }
        for (version, at) in other.daily {
            if let Some(known) = self.daily.iter_mut().find(|(v, _)| *v == version) {
                known.1 = known.1.max(at);
            } else {
                self.daily.push((version, at));
            }
        }
        self.daily.sort_by_key(|(_, at)| *at);
        self.daily.drain(..self.daily.len().saturating_sub(DAILY));
        self.last_repeat = self.last_repeat.max(other.last_repeat);
        if other.last.as_ref().is_some_and(|incoming| {
            self.last
                .as_ref()
                .is_none_or(|held| incoming.version > held.version)
        }) {
            self.last = other.last;
        }
    }

    pub(super) fn has_requests(&self) -> bool {
        !self.hints.is_empty()
    }

    pub(super) fn clear_hints(&mut self) {
        self.hints.clear();
        self.repeat = None;
    }

    fn note(&mut self, hint: Hint, reserved: Option<ListVersion>) {
        self.hints
            .retain(|previous| previous.version != hint.version);
        self.hints.push(hint);
        // Keep all tracked offers, the unsettled original, a request naming none,
        // and the most recent sixteen other versions from this recipient.
        let mut others = 0;
        self.hints.reverse();
        self.hints.retain(|hint| {
            if hint.version.is_none()
                || hint.version == reserved
                || self
                    .offers
                    .iter()
                    .any(|offer| Some(offer.version) == hint.version)
            {
                return true;
            }
            others += 1;
            others <= OTHER
        });
        self.hints.reverse();
    }

    fn remember(&mut self, version: ListVersion, at: u64) {
        if let Some(offer) = self
            .offers
            .iter_mut()
            .find(|offer| offer.version == version)
        {
            offer.answered = Some(offer.answered.map_or(at, |held| held.max(at)));
        } else {
            let mut latest = at;
            self.other.retain(|(v, held)| {
                if *v == version {
                    latest = latest.max(*held);
                }
                *v != version
            });
            self.other.push((version, latest));
            self.other.sort_by_key(|(_, at)| *at);
            if self.other.len() > OTHER {
                self.other.remove(0);
            }
        }
    }

    fn prune(&mut self, now: u64) {
        self.daily.retain(|(_, at)| now < at.saturating_add(DAY));
        self.other
            .retain(|(_, at)| now < at.saturating_add(PRIVATE_LIST_MAX_AGE_MS));
        let expired = self
            .offers
            .extract_if(.., |offer| {
                now >= offer.expires.saturating_add(PRIVATE_LIST_MAX_AGE_MS)
            })
            .filter_map(|o| Some((o.version, o.answered?)))
            .collect::<Vec<_>>();
        for (version, at) in expired {
            if now < at.saturating_add(PRIVATE_LIST_MAX_AGE_MS) {
                self.remember(version, at);
            }
        }
    }

    pub(super) fn first(&self, issue: &ListReservation) -> Option<u64> {
        self.offers
            .last()
            .filter(|offer| {
                offer.version.generation() == issue.generation
                    && offer.version.counter() == issue.counter
                    && offer.id == issue.id
                    && offer.hash == issue.hash
            })
            .map(|offer| offer.first)
    }

    pub(super) fn observe(
        &mut self,
        issue: &ListReservation,
        first: u64,
        same_choice: bool,
    ) -> Result<bool> {
        let version = ListVersion::new(issue.generation, issue.counter)?;
        if self.offers.last().is_some_and(|o| o.version >= version) {
            return Ok(false);
        }
        self.prune(first);
        self.repeat = None;
        if issue.answer.requested {
            self.daily.push((version, first));
        }
        if let Some(request) = &issue.answer.last {
            self.last = Some(LastAnswer {
                request: request.clone(),
                version,
                id: issue.id.clone(),
                hash: issue.hash,
            });
        }
        for &named in &issue.answer.named {
            self.remember(named, first);
        }
        if same_choice {
            self.hints.retain(|hint| hint.seq > issue.answer.through);
        }
        let mut answered = None;
        self.other.retain(|(named, at)| {
            if *named == version {
                answered = Some(*at);
            }
            *named != version
        });
        self.offers.push(Offer {
            version,
            id: issue.id.clone(),
            hash: issue.hash,
            first,
            expires: issue.expires,
            answered,
        });
        Ok(true)
    }

    fn needs_newer(&self, hint: &Hint, newest: ListVersion) -> bool {
        let Some(version) = hint.version else {
            return true;
        };
        if self.other.iter().any(|(named, _)| *named == version) {
            return false;
        }
        self.offers
            .iter()
            .find(|offer| offer.version == version)
            .is_none_or(|offer| {
                let expires = offer
                    .expires
                    .min(offer.first.saturating_add(PRIVATE_LIST_MAX_AGE_MS));
                offer.answered.is_none() && (version == newest || hint.at >= expires)
            })
    }

    pub(super) fn decision(
        &mut self,
        now: u64,
        newest: ListVersion,
        missing: Option<MissingAnswers>,
    ) -> Reply {
        self.prune(now);
        let unknown = missing
            .filter(|gap| now < gap.seen.saturating_add(DAY))
            .map_or(0, |gap| gap.possible);
        let capacity = self.daily.len() + unknown < DAILY;
        let mut repeat = self.repeat.is_some();
        for hint in &self.hints {
            if !self.needs_newer(hint, newest) {
                repeat = true;
            } else if capacity && !missing.is_some_and(|gap| gap.holds(hint, now)) {
                return Reply::Newer;
            }
        }
        if repeat {
            Reply::Repeat
        } else {
            Reply::Wait
        }
    }

    /// A repeat satisfies live older or already answered hints while an independent
    /// request for a newer issue may still be waiting for daily capacity.
    pub(super) fn take_repeat_hints(&mut self, newest: ListVersion) -> bool {
        let mut repeating = false;
        for hint in std::mem::take(&mut self.hints) {
            if self.needs_newer(&hint, newest) {
                self.hints.push(hint);
            } else {
                repeating = true;
            }
        }
        repeating
    }

    pub(super) fn bound_repeat(&mut self, now: u64) {
        self.last_repeat = self.last_repeat.max(Some(now));
    }

    fn prepare_repeat(
        &mut self,
        issue: &ListReservation,
        now: u64,
        previous: Option<u64>,
        last_repeat: Option<u64>,
    ) -> Result<bool> {
        if let Some(pending) = self.repeat.take() {
            if pending.id == issue.id && pending.hash == issue.hash && previous <= pending.previous
            {
                self.repeat = Some(pending);
                return Ok(true);
            }
        }
        let version = ListVersion::new(issue.generation, issue.counter)?;
        let repeating = self.take_repeat_hints(version);
        let last = self
            .last_repeat
            .max(last_repeat)
            .max(issue.missing.map(|gap| gap.seen));
        if !repeating || last.is_some_and(|at| now < at.saturating_add(HOUR)) {
            return Ok(false);
        }
        self.bound_repeat(now);
        self.repeat = Some(Repeat {
            id: issue.id.clone(),
            hash: issue.hash,
            previous,
        });
        Ok(true)
    }

    pub(super) fn answer(&self, requested: bool) -> RequestsAnswered {
        RequestsAnswered {
            through: self.hints.iter().map(|h| h.seq).max().unwrap_or(0),
            last: self.hints.iter().max_by_key(|h| h.seq).cloned(),
            named: self.hints.iter().filter_map(|h| h.version).collect(),
            requested,
        }
    }
}

pub(crate) fn note(
    cx: &mut ReceiveContext<'_>,
    stored: &StoredMessage,
    scopes: &[String],
    request: &PrivatePaymentListRequest,
    seq: u64,
) -> std::result::Result<(), Fail> {
    if !cx.cert().holds_scope(SCOPE) || request.app_id.is_some_and(|app| app != cx.app_id()) {
        return Ok(());
    }
    let sender = stored.sender();
    if sender == cx.identity() || !crate::receive::admits_new_state(cx, &sender, scopes, stored)? {
        return Ok(());
    }
    let at = key::list_recipient(&sender);
    let mut recipient = match state::cx_get::<ListRecipient>(cx, &at)? {
        Some(recipient) if recipient.chosen => recipient,
        None if request.held_version.is_some()
            && state::cx_get::<crate::hold::RestoreHold>(cx, key::RESTORE_HOLD)?.is_some()
            && state::cx_get::<ListSettings>(cx, key::LIST_SETTINGS)?.is_none() =>
        {
            let recipient = ListRecipient::restored(sender).map_err(|_| Fail::corrupt())?;
            keep_local(cx, local(&at, &recipient)?);
            return Ok(());
        }
        _ => return Ok(()),
    };
    let held = state::cx_get::<ListReservation>(cx, &key::list_reservation(&sender))?;
    let reserved = held
        .map(|r| ListVersion::new(r.generation, r.counter))
        .transpose()?;
    let accepted = stored.accepted_at().ok_or_else(Fail::corrupt)?;
    recipient.answers.note(
        Hint {
            version: request.held_version,
            id: stored.message().msg_id.clone(),
            hash: stored.hash(),
            at: accepted,
            seq,
        },
        reserved,
    );
    keep_local(cx, local(&at, &recipient)?);
    Ok(())
}

impl Paykit {
    /// Reconcile the local accepted issue with the permanent counter before spending
    /// any answer capacity. Its signed record identifies a sibling's publication,
    /// but only the installation holding the original can resend it unchanged.
    pub(super) async fn current_list_for_request(
        &mut self,
        prior: &ListReservation,
        accepted: bool,
    ) -> Result<(ListVersion, Option<MissingAnswers>)> {
        let local = ListVersion::new(prior.generation, prior.counter)?;
        self.own_identity_confirmed(false).await?;
        if self.app.own_identity().generation() != prior.generation {
            return Ok((local, prior.missing));
        }
        let target = target(&self.app_id(), &prior.recipient, prior.generation);
        let RecordRead::Fresh(held) = self.app.read_record_fresh(&target).await? else {
            return Err(counter_unavailable().into());
        };
        let current = self.publication_at(&held).await?;
        if prior.matches(&current) {
            return Ok((local, prior.missing));
        }
        let at = key::publication_history(&held.key);
        let mut history = self
            .local_get::<super::PublicationHistory>(&at)
            .await?
            .ok_or_else(counter_unavailable)?;
        let seen = match history.missing_since {
            Some(seen) => seen,
            None => {
                let seen = self.service_time().ok_or_else(counter_unavailable)?;
                history.missing_since = Some(seen);
                self.local_put(&at, &history).await?;
                seen
            }
        };
        Ok((
            ListVersion::new(current.generation, current.counter)?,
            MissingAnswers::after(
                Some(prior),
                current.generation,
                current.counter,
                seen,
                accepted,
            )?,
        ))
    }

    /// A restore can cross a provider generation. A new counter starts at one, while
    /// the recipient's answer limits still cover the previous generation's sends.
    pub(super) async fn earlier_list_answers(
        &self,
        recipient: &ListRecipient,
        current: Option<MissingAnswers>,
        now: u64,
    ) -> Result<Option<MissingAnswers>> {
        let Some((generation, restored)) = recipient.restored_generation else {
            return Ok(current);
        };
        if now >= restored.saturating_add(2 * PRIVATE_LIST_MAX_AGE_MS) {
            return Ok(current);
        }
        let mut possible = 0usize;
        let mut through = None;
        if self
            .local_get::<bool>(key::LIST_DISCOVERY)
            .await?
            .unwrap_or(false)
        {
            // The still incomplete inventory can contain an earlier key for this recipient.
            possible = DAILY;
            through = Some(ListVersion::new(
                generation - 1,
                pubky_switch_core::json::MAX_INT,
            )?);
        } else {
            for record in self.app.records_of(COLLECTION).await? {
                let Some(past) = record
                    .policy
                    .permanent
                    .as_ref()
                    .and_then(|p| p.generation)
                    .filter(|past| *past < generation)
                else {
                    continue;
                };
                let expected = target(&self.app_id(), &recipient.recipient, past);
                if self.app.record_key(&expected)? != record.key {
                    continue;
                }
                let known = recipient
                    .answers
                    .offers
                    .iter()
                    .filter(|offer| offer.version.generation() == past)
                    .map(|offer| offer.version.counter())
                    .max()
                    .unwrap_or(0);
                let counter = match self.publication_at(&record).await {
                    Ok(issue) => issue.counter,
                    Err(error) if crate::hold::local_failure(&error) => return Err(error),
                    Err(_) => pubky_switch_core::json::MAX_INT,
                };
                if counter > known {
                    through = through.max(Some(ListVersion::new(past, counter)?));
                    possible = DAILY.min(possible + (counter - known).min(DAILY as u64) as usize);
                }
            }
        }
        let Some(through) = through else {
            return Ok(current);
        };
        let earlier = if now < restored.saturating_add(DAY) {
            possible
        } else {
            0
        };
        let recent = current
            .filter(|gap| now < gap.seen.saturating_add(DAY))
            .map_or(0, |gap| gap.possible);
        Ok(Some(MissingAnswers {
            through: current.map_or(through, |gap| through.max(gap.through)),
            seen: current.map_or(restored, |gap| restored.max(gap.seen)),
            possible: DAILY.min(recent + earlier),
        }))
    }

    /// A reservation enters the outbox on application. Only the recipient's verified
    /// inbox acceptance makes it a version sent, including after the native sent copy ages out.
    pub(super) async fn observe_list_acceptance(
        &mut self,
        recipient: &mut ListRecipient,
        issue: &ListReservation,
    ) -> Result<Option<u64>> {
        if let Some(first) = recipient.answers.first(issue) {
            return Ok(Some(first));
        }
        if !matches!(
            self.app.delivery(&issue.id).await?,
            Some(Delivery::Accepted { .. })
        ) {
            return Ok(None);
        }
        let accepted = self
            .app
            .sent_to(&recipient.recipient, 0, u64::MAX)
            .await?
            .into_iter()
            .find(|sent| sent.msg_id == issue.id && sent.plaintext.hash() == issue.hash)
            .and_then(|sent| sent.accepted_at_ms);
        if let Some(first) = accepted {
            if recipient
                .answers
                .observe(issue, first, issue.choice == recipient.choice)?
            {
                self.local_put(&key::list_recipient(&recipient.recipient), recipient)
                    .await?;
            }
        }
        Ok(accepted)
    }

    /// A repeat can expose only the newest signed version of this recipient's
    /// current permanent counter, including after another installation published.
    pub(super) async fn repeat_list(
        &mut self,
        recipient: &mut ListRecipient,
        issue: &ListReservation,
        now: u64,
        report: Option<&ReceiveReport>,
    ) -> Result<()> {
        self.refuse_while_sends_held(&recipient.recipient).await?;
        if !matches!(
            self.app.delivery(&issue.id).await?,
            Some(Delivery::Accepted { .. })
        ) {
            return Ok(());
        }
        self.own_identity_confirmed(false).await?;
        let generation = self.app.own_identity().generation();
        if issue.generation > generation
            || (issue.generation < generation
                && self.newer_list_reserved(issue, generation, report).await?)
        {
            return Err(counter_unavailable().into());
        }
        let target = target(&self.app_id(), &recipient.recipient, issue.generation);
        let RecordRead::Fresh(held) = self.app.read_record_fresh(&target).await? else {
            return Err(counter_unavailable().into());
        };
        if !issue.matches(&self.publication_at(&held).await?) {
            return Err(counter_unavailable().into());
        }
        let sent = self.app.sent_to(&recipient.recipient, 0, u64::MAX).await?;
        let last_repeat = sent
            .iter()
            .filter(|sent| sent.kind == Kind::PrivatePaymentList.as_str())
            .flat_map(|sent| sent.resent_at_ms.iter().copied())
            .max();
        let latest = sent
            .iter()
            .find(|sent| sent.msg_id == issue.id && sent.plaintext.hash() == issue.hash)
            .and_then(|sent| sent.resent_at_ms.last().copied());
        let at = key::list_recipient(&recipient.recipient);
        let due = recipient
            .answers
            .prepare_repeat(issue, now, latest, last_repeat)?;
        self.local_put(&at, recipient).await?;
        if !due {
            return Ok(());
        }
        self.own_identity_confirmed(false).await?;
        if self.app.own_identity().generation() != generation || !self.holds(SCOPE) {
            return Err(counter_unavailable().into());
        }
        match self.send_again(&issue.id).await {
            Ok(_) => {}
            Err(AppError::Refused(Refusal::WontDeliver(WontDeliver::GivenUp))) => {
                recipient.answers.repeat = None;
                self.local_put(&at, recipient).await?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(counter: u64) -> ListVersion {
        ListVersion::new(1, counter).unwrap()
    }

    fn hint(counter: u64, seq: u64) -> Hint {
        Hint {
            version: Some(version(counter)),
            id: format!("00000000-0000-4000-8000-{seq:012x}"),
            hash: [0; 32],
            seq,
            at: DAY,
        }
    }

    #[test]
    fn test_list_answers_remember_tracked_versions_and_sixteen_others() {
        let mut answers = Answers::default();
        let original = hint(1, 1);
        answers.offers.push(Offer {
            version: version(1),
            id: original.id.clone(),
            hash: original.hash,
            first: 0,
            expires: DAY,
            answered: None,
        });
        for named in 1..=18 {
            answers.remember(version(named), DAY + named);
        }
        assert_eq!(answers.other.len(), 16);
        let newest = version(19);
        for (named, repeats) in [(1, true), (2, false), (3, true), (18, true), (19, false)] {
            answers.clear_hints();
            answers.note(hint(named, 20), Some(newest));
            assert_eq!(
                matches!(answers.decision(2 * DAY, newest, None), Reply::Repeat),
                repeats
            );
        }
        // An untracked name ages out 30 days after its answer. The earlier
        // tracked issue stays remembered for the rest of its retention.
        answers.clear_hints();
        answers.note(hint(18, 21), None);
        assert!(matches!(
            answers.decision(DAY + 18 + PRIVATE_LIST_MAX_AGE_MS, newest, None),
            Reply::Newer
        ));
    }

    #[test]
    fn test_list_answers_keep_equal_time_charges_and_the_unobserved_counter_gap() {
        let mut answers = Answers::default();
        let mut other = Answers::default();
        answers.daily.push((version(1), DAY));
        other.daily.push((version(2), DAY));
        answers.retain_history(other);
        assert_eq!(answers.daily.len(), 2);
        let gap = MissingAnswers::after(None, 1, 2, DAY, false).unwrap();
        let mut request = hint(3, 4);
        request.version = None;
        answers.note(request, None);
        assert!(matches!(
            answers.decision(DAY - 1, version(3), gap),
            Reply::Wait
        ));
        assert!(matches!(
            answers.decision(2 * DAY - 1, version(3), gap),
            Reply::Wait
        ));
        assert!(matches!(
            answers.decision(2 * DAY, version(3), gap),
            Reply::Newer
        ));
        answers.clear_hints();
        answers.note(hint(2, 5), None);
        assert!(matches!(
            answers.decision(2 * DAY, version(3), gap),
            Reply::Wait
        ));
        assert!(matches!(
            answers.decision(DAY + 2 * PRIVATE_LIST_MAX_AGE_MS, version(3), gap),
            Reply::Newer
        ));
    }
}
