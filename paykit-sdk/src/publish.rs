//! Publishing a Private Payment List to a chosen counterparty (Paykit spec 8.4.1).
//!
//! A native batch keeps the original message's hash, recipient and generation in a
//! protocol-local key naming the batch. Its application records the sharing decision
//! and queues the signed message together. If the application is undecided, changes
//! for that recipient wait while other recipients can proceed. An in-place restore
//! retains outstanding originals from both stores and requires a fresh sharing choice.

pub(crate) mod answers;
pub(crate) mod history;
mod withdrawal;

use history::PublicationHistory;
use std::collections::{BTreeMap, BTreeSet};
pub(crate) use withdrawal::FirstDelivery;
use withdrawal::{Payable, Withdrawal};

use paykit_lib::bodies::{ListEndpoint, PrivatePaymentList, PRIVATE_LIST_MAX_AGE_MS};
use paykit_lib::derive::natural_key;
use paykit_lib::records::{ListPublicationRecord, UnknownMembers};
use paykit_lib::{Kind, ListVersion, PaymentEndpointIdentifier, PaymentEndpointPayload};
use pubky_switch_client::app::{
    Collection, DeliveryView, KeptBatchDrop, KeptBatchStatus, KeptReason, LocalWrite, OpBatch,
    OutgoingMessage, ReceiveReport, RecordContent, RecordRead, RecordTarget, SharedRecord,
    StoreFailure, UserConfirmed,
};
use pubky_switch_core::ident::Policy;
use pubky_switch_core::ops::Condition;
use pubky_switch_core::records::DAY_MS as DAY;

use crate::error::{Refusal, Result};
use crate::state::{self, key, PublicationIds, PublicationKey, Tx};
use crate::Paykit;
use pubky_switch_core::enc::random_bytes;
use serde::{Deserialize, Serialize};

pub(crate) const COLLECTION: &str = "private_list_publications";
const SCOPE: &str = "paykit_request";
const HOUR: u64 = DAY / 24;

#[derive(Clone, Copy, PartialEq, Eq)]
enum IssueFor {
    Change,
    Answer(ListVersion),
    RetryWithdrawal,
    /// A new version the user confirmed after the current one's delivery ended unknown,
    /// built on the current counter.
    Confirmed,
}

pub(crate) fn collection() -> Collection {
    Collection::with_permanent_generations(COLLECTION, SCOPE, &[SCOPE])
}

fn target(publisher: &[u8; 16], recipient: &[u8; 32], generation: u64) -> RecordTarget {
    let policy = Policy::new(SCOPE, &[SCOPE]).permanent(Some(generation));
    RecordTarget::new(
        Collection::new(COLLECTION, policy),
        &natural_key::list_publication(publisher, recipient, generation),
    )
}

fn counter_unavailable() -> Refusal {
    Refusal::NotNow(
        "Paykit spec 8.4.1: the recipient's publication counter cannot be verified".into(),
    )
}

/// A native cache mark for identifying a foreign replacement of this app's counter.
/// It supplies neither a version nor a recipient. The qualified history is owner-local.
pub(crate) fn own_counter_mark(writer: [u8; 16], value: &[u8]) -> Option<Vec<u8>> {
    let record = ListPublicationRecord::read(value).ok()?;
    (record.publisher == writer).then(|| vec![1])
}

/// The client calls this only for a verified write by a different app. The publishing
/// app's own value can therefore never be taken from such a write, even on its first read.
pub(crate) fn foreign_publication(
    own: Option<[u8; 16]>,
    seen: Option<&[u8]>,
    value: Option<&[u8]>,
) -> Option<String> {
    let current = match ListPublicationRecord::read(value?) {
        Ok(current) => current,
        Err(error) => return Some(error.to_string()),
    };
    if own == Some(current.publisher) || seen == Some(&[1]) {
        return Some("Paykit wire 5.8: the write is not by the publisher of this counter".into());
    }
    None
}

fn unresolved() -> Refusal {
    Refusal::NotNow("Paykit spec 8.4.1: the recipient's previous publication is unresolved".into())
}

/// The revision of a reusable list is derived from its endpoints, so the same list set
/// again is the same revision: a change that is undone before it went out sends nothing
/// in place of a list of unknown delivery (Paykit spec 8.4).
fn list_revision(endpoints: &BTreeMap<String, String>) -> [u8; 16] {
    let mut parts: Vec<&[u8]> = vec![b"paykit/v1/list-revision"];
    for (id, payload) in endpoints {
        parts.extend([id.as_bytes(), payload.as_bytes()]);
    }
    let digest = pubky_switch_core::enc::hash(&pubky_switch_core::enc::lp(&parts));
    let mut revision = [0; 16];
    revision.copy_from_slice(&digest[..16]);
    revision
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListSettings {
    endpoints: BTreeMap<String, String>,
}

impl ListSettings {
    /// Derived on every read, never stored, so no stored value can disagree with it.
    fn revision(&self) -> [u8; 16] {
        list_revision(&self.endpoints)
    }

    fn endpoints(&self) -> Result<BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>> {
        self.endpoints
            .iter()
            .map(|(id, payload)| {
                Ok((
                    PaymentEndpointIdentifier::parse(id)?,
                    PaymentEndpointPayload::parse(payload)?,
                ))
            })
            .collect()
    }

    pub(crate) fn check(&self) -> Result<()> {
        self.endpoints().map(|_| ())
    }
}

/// A counterparty's current Private Payment List that the client gave up with an
/// unknown outcome (Paykit spec 8.4, 9). It may or may not have reached the
/// counterparty. Show it before asking the user whether to send another
/// ([`Paykit::send_list_again`]); it comes from [`Refusal::ListDeliveryUnknown`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnknownListDelivery {
    /// The counterparty the list was sent to.
    pub recipient: [u8; 32],
    /// The list's version.
    pub version: ListVersion,
    /// The list message's `msg_id`.
    pub msg_id: String,
    /// Whether the list was empty: a withdrawal. If it didn't arrive, the counterparty
    /// may keep paying the earlier list. A nonempty list that didn't arrive leaves the
    /// counterparty without it.
    pub withdrawal: bool,
    /// For a withdrawal, the latest time, in Unix milliseconds, an earlier nonempty list
    /// to this counterparty can still be paid; `None` when it can't be bounded. For a
    /// nonempty list, when the list itself expires.
    pub payable_until_ms: Option<u64>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListRecipient {
    pub(crate) recipient: [u8; 32],
    chosen: bool,
    choice: [u8; 16],
    withdraw: bool,
    answers: answers::Answers,
    await_repeat_time: bool,
    #[serde(deserialize_with = "Option::deserialize")]
    restored_generation: Option<(u64, u64)>,
}

impl ListRecipient {
    pub(crate) fn check(&self) -> Result<()> {
        if self.restored_generation.is_some_and(|(generation, _)| {
            !(2..=pubky_switch_core::json::MAX_INT).contains(&generation)
        }) {
            return Err(crate::PaykitError::corrupt());
        }
        self.answers.check()
    }

    fn choose(previous: Option<Self>, recipient: [u8; 32], chosen: bool) -> Result<Self> {
        let mut next = Self::new(recipient, chosen)?;
        if let Some(previous) = previous {
            next.answers = previous.answers;
            next.await_repeat_time = previous.await_repeat_time;
            next.restored_generation = previous.restored_generation;
            next.answers.clear_hints();
        }
        Ok(next)
    }

    fn restored(recipient: [u8; 32]) -> Result<Self> {
        let mut state = Self::new(recipient, false)?;
        state.await_repeat_time = true;
        Ok(state)
    }

    fn new(recipient: [u8; 32], chosen: bool) -> Result<Self> {
        Ok(Self {
            recipient,
            chosen,
            choice: random_bytes()?,
            withdraw: !chosen,
            answers: answers::Answers::default(),
            await_repeat_time: false,
            restored_generation: None,
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListReservation {
    answer: answers::RequestsAnswered,
    expires: u64,
    pub(crate) publisher: [u8; 16],
    predecessors: Payable,
    delivery: FirstDelivery,
    /// The user confirmed another version after this list's delivery ended unknown. It
    /// is kept until that version is reserved, so a receive can complete it.
    confirmed: bool,
    #[serde(deserialize_with = "Option::deserialize")]
    withdrawal: Option<Withdrawal>,
    #[serde(deserialize_with = "Option::deserialize")]
    missing: Option<answers::MissingAnswers>,
    pub(crate) recipient: [u8; 32],
    choice: [u8; 16],
    #[serde(deserialize_with = "Option::deserialize")]
    revision: Option<[u8; 16]>,
    generation: u64,
    counter: u64,
    id: String,
    hash: [u8; 32],
}

impl ListReservation {
    pub(crate) fn check(&self) -> Result<()> {
        let version = ListVersion::new(self.generation, self.counter)?;
        if self.revision.is_some() && self.withdrawal.is_some()
            || self.confirmed && self.delivery != FirstDelivery::Unknown
            || self.withdrawal.is_some_and(|w| w.first > w.issued)
        {
            return Err(crate::PaykitError::corrupt());
        }
        if let Some(missing) = self.missing {
            missing.check(version)?;
        }
        pubky_switch_core::json::check_uuid_v4(&self.id)?;
        self.answer.check()
    }

    fn original(&self) -> PublicationKey {
        PublicationKey {
            recipient: self.recipient,
            generation: self.generation,
            hash: self.hash,
        }
    }

    /// The same signed issue: its message and counter.
    fn matches_issue(&self, other: &Self) -> bool {
        self.id == other.id
            && self.hash == other.hash
            && self.generation == other.generation
            && self.counter == other.counter
    }

    fn matches(&self, record: &ListPublicationRecord) -> bool {
        self.publisher == record.publisher
            && self.recipient == record.recipient
            && self.id == record.msg_id
            && self.hash == record.message_hash
            && self.generation == record.generation
            && self.counter == record.counter
    }
}

/// A restore retains outstanding originals from both installations. A matching
/// applied write settles its signed message without restoring permission to share.
#[derive(Default)]
pub(crate) struct RestoredPublications {
    batches: BTreeMap<PublicationKey, PublicationIds>,
    applied: BTreeSet<PublicationKey>,
    issues: BTreeMap<([u8; 32], ListVersion), (String, [u8; 32])>,
    floors: BTreeMap<[u8; 32], ListReservation>,
    observed: BTreeMap<Vec<u8>, PublicationHistory>,
    recipients: BTreeSet<[u8; 32]>,
    answers: BTreeMap<[u8; 32], answers::Answers>,
}

impl RestoredPublications {
    /// Compare every qualified issue before coalescing either source's per-recipient floor.
    fn remember_issue(
        &mut self,
        recipient: [u8; 32],
        generation: u64,
        counter: u64,
        id: &str,
        hash: [u8; 32],
    ) -> Result<()> {
        let version = ListVersion::new(generation, counter)?;
        match self.issues.entry((recipient, version)) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((id.to_owned(), hash));
            }
            std::collections::btree_map::Entry::Occupied(entry)
                if entry.get().0 != id || entry.get().1 != hash =>
            {
                return Err(Refusal::NotNow(format!(
                    "Paykit wire 5.8: different signed lists at generation {generation} counter {counter} for recipient {}",
                    pubky_switch_core::enc::zbase32_encode(&recipient),
                )).into());
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn pending(
        &mut self,
        key: &[u8],
        value: &[u8],
        binder: &state::Binder<'_>,
    ) -> Result<()> {
        let (original, ids) = state::publication_entry(key, value, binder.identity())?;
        self.recipients.insert(original.recipient);
        self.batches.entry(original).or_default().extend(ids);
        Ok(())
    }

    pub(crate) fn applied(
        &mut self,
        key: &[u8],
        value: &[u8],
        binder: &state::Binder<'_>,
    ) -> Result<()> {
        let mut issue = state::restored::<ListReservation>(key, value, binder)?;
        // A restore gives every counterparty a fresh choice: no confirmation carries over.
        issue.confirmed = false;
        self.remember_issue(
            issue.recipient,
            issue.generation,
            issue.counter,
            &issue.id,
            issue.hash,
        )?;
        self.recipients.insert(issue.recipient);
        self.applied.insert(issue.original());
        if let Some(held) = self.floors.get_mut(&issue.recipient) {
            let prior = (held.generation, held.counter);
            let next = (issue.generation, issue.counter);
            match next.cmp(&prior) {
                std::cmp::Ordering::Less => {
                    held.predecessors = held.predecessors.union(issue.payable_after(None));
                    return Ok(());
                }
                std::cmp::Ordering::Equal => {
                    held.predecessors = held.predecessors.union(issue.predecessors);
                    held.delivery = held.delivery.with(issue.delivery);
                    if issue.withdrawal.is_none() {
                        held.withdrawal = None;
                    }
                    return Ok(());
                }
                std::cmp::Ordering::Greater => {
                    issue.predecessors = issue.predecessors.union(held.payable_after(None));
                }
            }
        }
        self.floors.insert(issue.recipient, issue);
        Ok(())
    }

    pub(crate) fn observed(
        &mut self,
        key: &[u8],
        bytes: &[u8],
        binder: &state::Binder<'_>,
    ) -> Result<()> {
        let history = state::restored::<PublicationHistory>(key, bytes, binder)?;
        for issue in history.qualified_issues() {
            self.remember_issue(
                issue.recipient,
                issue.generation,
                issue.counter,
                &issue.msg_id,
                issue.message_hash,
            )?;
        }
        self.recipients.insert(history.floor.recipient);
        if let Some(held) = self.observed.get_mut(key) {
            held.merge(history)?;
        } else {
            self.observed.insert(key.to_vec(), history);
        }
        Ok(())
    }

    pub(crate) fn writes(mut self) -> Result<Vec<LocalWrite>> {
        let mut writes = Vec::new();
        for (original, ids) in self.batches {
            if !self.applied.contains(&original) {
                writes.push(original.write(&ids)?);
            }
        }
        for (key, history) in self.observed {
            writes.push(LocalWrite::Put {
                key,
                value: state::encode(&history)?,
            });
        }
        for (recipient, issue) in self.floors {
            writes.push(state::local_put(
                &key::list_reservation(&recipient),
                &issue,
            )?);
        }
        for recipient in self.recipients {
            let mut restored = ListRecipient::restored(recipient)?;
            restored.answers = self.answers.remove(&recipient).unwrap_or_default();
            writes.push(state::local_put(
                &key::list_recipient(&recipient),
                &restored,
            )?);
        }
        Ok(writes)
    }

    pub(crate) fn recipient(&mut self, recipient: ListRecipient) -> Result<()> {
        for (version, id, hash) in recipient.answers.issues() {
            self.remember_issue(
                recipient.recipient,
                version.generation(),
                version.counter(),
                id,
                hash,
            )?;
        }
        self.recipients.insert(recipient.recipient);
        self.answers
            .entry(recipient.recipient)
            .or_default()
            .retain_history(recipient.answers);
        Ok(())
    }
}

pub(crate) fn may_send(view: &DeliveryView<'_>) -> std::result::Result<bool, StoreFailure> {
    fn allowed(view: &DeliveryView<'_>) -> Result<bool> {
        let message = view.message();
        let local = view.local();
        if !view.cert().holds_scope(SCOPE) {
            return Ok(false);
        }
        let Some(recipient) =
            crate::state::view_get::<ListRecipient>(local, &key::list_recipient(&message.to))?
        else {
            return Ok(false);
        };
        let Some(issue) =
            crate::state::view_get::<ListReservation>(local, &key::list_reservation(&message.to))?
        else {
            return Ok(false);
        };
        if issue.choice != recipient.choice
            || issue.id != message.msg_id
            || issue.hash != view.hash()
        {
            return Ok(false);
        }
        let list = PrivatePaymentList::from_body(&message.body)?;
        if list.expires_at <= view.now_ms() {
            return Ok(false);
        }
        if list.payment_endpoints.is_empty() {
            return Ok(issue.revision.is_none());
        }
        let settings = crate::state::view_get::<ListSettings>(local, key::LIST_SETTINGS)?;
        if !recipient.chosen
            || recipient.withdraw
            || !settings.is_some_and(|current| Some(current.revision()) == issue.revision)
        {
            return Ok(false);
        }
        if !crate::receive::reads_lists(view.cert().scopes()) {
            return Ok(true);
        }
        if crate::state::view_get::<u64>(local, key::LISTS_READ)?.is_none() {
            return Ok(false);
        }
        if let Some(mark) = crate::state::view_get::<crate::state::OwnBlockMark>(
            local,
            &key::own_block(&message.to),
        )? {
            let kept = match mark.unblock_batch() {
                Some(id) => matches!(view.kept_batch(id)?, Some(KeptBatchStatus::Kept { .. })),
                None => false,
            };
            return Ok(mark.counts(kept) == Some(false));
        }
        Ok(
            !crate::state::view_get::<Vec<[u8; 32]>>(local, key::DENIED)?
                .is_some_and(|ids| ids.contains(&message.to)),
        )
    }
    allowed(view).map_err(crate::protocol::view_failure)
}

fn current_publication(record: &SharedRecord) -> Result<ListPublicationRecord> {
    if !record.authenticated
        || record.flagged
        || record.not_taken
        || record.different_write_at_held_version
        || record.divergent
        || record.doubtful
    {
        return Err(counter_unavailable().into());
    }
    let bytes = record.content.value().ok_or_else(counter_unavailable)?;
    let publication = ListPublicationRecord::read(bytes)?;
    if record.writer != publication.publisher {
        return Err(Refusal::NotNow(format!(
            "Paykit wire 5.8: app {} wrote another publisher's counter",
            pubky_switch_core::enc::zbase32_encode(&record.writer),
        ))
        .into());
    }
    Ok(publication)
}

fn bound_publication(
    record: &SharedRecord,
    derive: impl FnOnce(
        &RecordTarget,
    ) -> std::result::Result<[u8; 32], pubky_switch_client::app::AppError>,
) -> Result<ListPublicationRecord> {
    let publication = current_publication(record)?;
    let expected = target(
        &publication.publisher,
        &publication.recipient,
        publication.generation,
    );
    if record.collection != COLLECTION
        || record.policy != expected.collection.policy
        || derive(&expected)? != record.key
    {
        return Err(counter_unavailable().into());
    }
    Ok(publication)
}

impl Paykit {
    async fn publication_at(&self, record: &SharedRecord) -> Result<ListPublicationRecord> {
        let current = bound_publication(record, |target| self.app.record_key(target))?;
        if current.publisher == self.app_id() {
            let history = self
                .local_get::<PublicationHistory>(&key::publication_history(&record.key))
                .await?
                .ok_or_else(counter_unavailable)?;
            history.check_current(&current)?;
            if let Some(applied) = self
                .local_get::<ListReservation>(&key::list_reservation(&current.recipient))
                .await?
                .filter(|issue| issue.generation == current.generation)
            {
                if applied.counter > current.counter
                    || (applied.counter == current.counter && !applied.matches(&current))
                {
                    return Err(counter_unavailable().into());
                }
            }
        }
        Ok(current)
    }

    fn later_recipient_key(&self, held: &SharedRecord, issue: &ListReservation) -> Result<bool> {
        let Some(generation) = held
            .policy
            .permanent
            .as_ref()
            .and_then(|p| p.generation)
            .filter(|generation| *generation > issue.generation)
        else {
            return Ok(false);
        };
        Ok(self
            .app
            .record_key(&target(&self.app_id(), &issue.recipient, generation))?
            == held.key)
    }

    /// The active generation has a different key for this recipient. For a jump
    /// across intervening generations, only a current inventory covers all of them.
    async fn newer_list_reserved(
        &mut self,
        issue: &ListReservation,
        generation: u64,
        report: Option<&ReceiveReport>,
    ) -> Result<bool> {
        let publisher = self.app_id();
        let active = target(&publisher, &issue.recipient, generation);
        let key = self.app.record_key(&active)?;
        match self.app.read_record_fresh(&active).await? {
            RecordRead::Fresh(record) => {
                self.publication_at(&record).await?;
                return Ok(true);
            }
            RecordRead::Kept {
                record: None,
                reason: KeptReason::NotFound,
            } if self
                .local_get::<PublicationHistory>(&key::publication_history(&key))
                .await?
                .is_none()
                && self.app.record_seen(&key).await?.is_none() => {}
            _ => return Err(counter_unavailable().into()),
        }
        if generation - issue.generation == 1 {
            return Ok(false);
        }
        if self
            .local_scan::<PublicationHistory>(key::PUBLICATION_HISTORY)
            .await?
            .into_iter()
            .any(|(_, history)| {
                history.floor.publisher == publisher
                    && history.floor.recipient == issue.recipient
                    && history.floor.generation > issue.generation
            })
        {
            return Ok(true);
        }
        let report = report.ok_or_else(counter_unavailable)?;
        let inventory = self
            .app
            .current_collection(&collection(), report)
            .await?
            .ok_or_else(counter_unavailable)?;
        for record in inventory.live {
            if self.later_recipient_key(&record, issue)? {
                self.publication_at(&record).await?;
                return Ok(true);
            }
        }
        for key in inventory.uncertain_keys {
            // The signed floors above already exclude a later issue at a known key.
            // An unknown opaque key could still belong to this recipient.
            if self
                .local_get::<PublicationHistory>(&key::publication_history(&key))
                .await?
                .is_none()
            {
                return Err(counter_unavailable().into());
            }
        }
        Ok(false)
    }

    /// Set the reusable Private Payment List. Each chosen contact is owed the changed
    /// list until its conditional publication applies (Paykit spec 8.4).
    pub async fn set_list_endpoints(
        &mut self,
        endpoints: BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
    ) -> Result<()> {
        self.first_read_before_writing().await?;
        self.refresh_own()?;
        if !self.holds(SCOPE) {
            return Err(Refusal::ScopeMissing(SCOPE.into()).into());
        }
        let endpoints = endpoints
            .into_iter()
            .map(|(id, payload)| (id.as_str().to_owned(), payload.as_str().to_owned()))
            .collect();
        if self
            .local_get::<ListSettings>(key::LIST_SETTINGS)
            .await?
            .is_some_and(|held| held.endpoints == endpoints)
        {
            return Ok(());
        }
        self.local_put(key::LIST_SETTINGS, &ListSettings { endpoints })
            .await?;
        Box::pin(self.tend_private_lists(None)).await
    }

    /// Share the current reusable list with the counterparty the host chose. If
    /// sends are held, choosing a new or withdrawn recipient requires another host call.
    ///
    /// # Errors
    ///
    /// [`Refusal::ListDeliveryUnknown`] when the counterparty's current list is one the
    /// client gave up with an unknown outcome: nothing replaces it without the user's
    /// confirmation ([`Paykit::send_list_again`]; Paykit spec 9). [`Refusal::NotNow`]
    /// while sends are held, before the list is set, or while an earlier publication is
    /// unresolved. A store or client failure.
    pub async fn publish_list_to(&mut self, recipient: [u8; 32]) -> Result<()> {
        self.first_read_before_writing().await?;
        self.refresh_own()?;
        if recipient == self.identity() || !self.holds(SCOPE) {
            return Err(Refusal::NotNow(
                "Paykit spec 8.4: a publishing app chooses a counterparty".into(),
            )
            .into());
        }
        if self
            .local_get::<ListSettings>(key::LIST_SETTINGS)
            .await?
            .is_none()
        {
            return Err(Refusal::NotNow(
                "Paykit spec 8.4: set the app's list before sharing it".into(),
            )
            .into());
        }
        let at = key::list_recipient(&recipient);
        let choice = match self.local_get::<ListRecipient>(&at).await? {
            Some(prior) if prior.chosen => prior,
            Some(mut prior) if self.list_is_empty().await? => {
                // Choosing again with an empty list would send the same empty content in
                // place of a withdrawal of unknown delivery: that takes the confirmation.
                if let Some((_, shown)) = self.unknown_issue(&mut prior).await? {
                    prior.chosen = true;
                    prior.withdraw = false;
                    self.local_put(&at, &prior).await?;
                    return Err(Refusal::ListDeliveryUnknown(Box::new(shown)).into());
                }
                self.refuse_while_sends_held(&recipient).await?;
                let chosen = ListRecipient::choose(Some(prior), recipient, true)?;
                self.local_put(&at, &chosen).await?;
                chosen
            }
            prior => {
                self.refuse_while_sends_held(&recipient).await?;
                let chosen = ListRecipient::choose(prior, recipient, true)?;
                self.local_put(&at, &chosen).await?;
                chosen
            }
        };
        Box::pin(self.publish_recipient(choice, None)).await?;
        self.refuse_unknown_list(&recipient).await
    }

    /// Stop sharing with this counterparty. A fresh, empty list supersedes what it may hold.
    /// If the client confirms first non-delivery, later receives can reserve a replacement
    /// after an hour, then hourly for the first day and daily while an earlier offer may be paid.
    /// Choosing the counterparty again ends this withdrawal.
    ///
    /// # Errors
    ///
    /// [`Refusal::ListDeliveryUnknown`] when the counterparty's current list is already
    /// an empty one whose delivery the client gave up with an unknown outcome, and the
    /// counterparty hasn't accepted it since. The counterparty is recorded as withdrawn
    /// all the same, but nothing is sent: pass the refusal's value to
    /// [`Paykit::send_list_again`] only once the user confirms another withdrawal
    /// (Paykit spec 8.4, 9). [`Refusal::NotNow`] while an earlier publication is
    /// unresolved, or while a confirmed list can't be published yet. A store or client
    /// failure.
    pub async fn withdraw_list_from(&mut self, recipient: [u8; 32]) -> Result<()> {
        self.first_read_before_writing().await?;
        self.refresh_own()?;
        let at = key::list_recipient(&recipient);
        let Some(mut prior) = self.local_get::<ListRecipient>(&at).await? else {
            return Ok(());
        };
        if prior.chosen {
            // An empty current list of unknown outcome already withdraws it, as one emptied
            // by `set_list_endpoints`: the counterparty stops being chosen under the same
            // choice, and nothing replaces that list without the user's confirmation.
            let unknown = self.unknown_issue(&mut prior).await?;
            let choice = prior.choice;
            let mut left = ListRecipient::choose(Some(prior), recipient, false)?;
            if let Some((_, shown)) = unknown.filter(|(_, shown)| shown.withdrawal) {
                left.choice = choice;
                left.withdraw = false;
                self.local_put(&at, &left).await?;
                return Err(Refusal::ListDeliveryUnknown(Box::new(shown)).into());
            }
            self.local_put(&at, &left).await?;
            return Box::pin(self.publish_recipient(left, None)).await;
        }
        Box::pin(self.publish_recipient(prior, None)).await?;
        self.refuse_unknown_list(&recipient).await
    }

    /// Whether the reusable list has no endpoints: sharing it sends an empty list.
    async fn list_is_empty(&self) -> Result<bool> {
        Ok(self
            .local_get::<ListSettings>(key::LIST_SETTINGS)
            .await?
            .is_some_and(|settings| settings.endpoints.is_empty()))
    }

    /// Report the counterparty's current list whose delivery is unknown, after a call
    /// that would otherwise answer without sending anything (Paykit spec 9).
    async fn refuse_unknown_list(&mut self, recipient: &[u8; 32]) -> Result<()> {
        let at = key::list_recipient(recipient);
        let Some(mut current) = self.local_get::<ListRecipient>(&at).await? else {
            return Ok(());
        };
        // A kept confirmation needs no check here: the publication before this tries it,
        // and either reserves its version or fails the call.
        match self.unknown_issue(&mut current).await? {
            Some((_, shown)) => Err(Refusal::ListDeliveryUnknown(Box::new(shown)).into()),
            None => Ok(()),
        }
    }

    /// Send the counterparty a newly signed version of its list after the user confirmed
    /// `shown`, from [`Refusal::ListDeliveryUnknown`]. The earlier list may already have
    /// reached the counterparty; the new one supersedes it. It is what the counterparty's
    /// current choice calls for: the reusable list if it is chosen and the list has
    /// endpoints, otherwise an empty one. The confirmation is kept with the shown list, and
    /// the version is built on the counter as read when it is reserved, whatever another
    /// installation or a move did to it since (Paykit spec 8.4, 8.4.1, 9).
    ///
    /// Returns once nothing of unknown outcome is left for the counterparty: the new
    /// version is queued, or the shown list was accepted, superseded or can no longer be
    /// paid since.
    ///
    /// # Errors
    ///
    /// [`Refusal::ListDeliveryUnknown`] with the current list when `shown` is no longer
    /// it; nothing is sent. [`Refusal::NotNow`] while the publication can't be made now,
    /// such as an unresolved earlier publication or held sends for a nonempty list: the
    /// confirmation is kept, and a later call or receive sends the list once it can.
    /// [`Refusal::NotFound`] for a
    /// counterparty never chosen. [`Refusal::ScopeMissing`] without `paykit_request`. A
    /// store or client failure.
    pub async fn send_list_again(
        &mut self,
        shown: &UnknownListDelivery,
        _confirmed: UserConfirmed,
    ) -> Result<()> {
        self.first_read_before_writing().await?;
        self.refresh_own()?;
        if !self.holds(SCOPE) {
            return Err(Refusal::ScopeMissing(SCOPE.into()).into());
        }
        let at = key::list_recipient(&shown.recipient);
        let Some(mut recipient) = self.local_get::<ListRecipient>(&at).await? else {
            return Err(Refusal::NotFound.into());
        };
        match self.unknown_issue(&mut recipient).await? {
            Some((mut prior, current)) if current == *shown => {
                prior.confirmed = true;
                self.local_put(&key::list_reservation(&shown.recipient), &prior)
                    .await?;
            }
            Some((_, current)) => {
                return Err(Refusal::ListDeliveryUnknown(Box::new(current)).into())
            }
            // Accepted, superseded or no longer payable: the publication follows the
            // current choice as at any call, and a kept confirmation completes.
            None => {}
        }
        // Kept first: an unresolved earlier publication answers "not now", and a later
        // call or receive sends the confirmed version.
        self.resolve_publications(&shown.recipient).await?;
        Box::pin(self.publish_recipient(recipient, None)).await?;
        self.refuse_unknown_list(&shown.recipient).await
    }

    pub(crate) async fn tend_private_lists(
        &mut self,
        report: Option<&ReceiveReport>,
    ) -> Result<()> {
        if !self.holds(SCOPE) {
            return Ok(());
        }
        if self
            .local_get::<bool>(key::LIST_DISCOVERY)
            .await?
            .unwrap_or(false)
        {
            let page = self.app.list_collection(&collection()).await?;
            let mut undiscovered = page.more;
            for held in self.app.records_of(COLLECTION).await? {
                let Ok(prior) = self.publication_at(&held).await else {
                    // A later key or a readable successor can still identify its recipient.
                    undiscovered |= held.content != RecordContent::Deleted;
                    continue;
                };
                if prior.publisher != self.app_id() {
                    continue;
                }
                let at = key::list_recipient(&prior.recipient);
                if self.local_get::<ListRecipient>(&at).await?.is_none() {
                    self.local_put(&at, &ListRecipient::restored(prior.recipient)?)
                        .await?;
                }
            }
            if !undiscovered {
                self.local_delete(key::LIST_DISCOVERY).await?;
            }
        }
        for (_, recipient) in self
            .local_scan::<ListRecipient>(key::LIST_RECIPIENTS)
            .await?
        {
            if let Err(error) = Box::pin(self.publish_recipient(recipient, report)).await {
                if crate::hold::local_failure(&error) {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    async fn publish_recipient(
        &mut self,
        mut recipient: ListRecipient,
        report: Option<&ReceiveReport>,
    ) -> Result<()> {
        let settings = self.local_get::<ListSettings>(key::LIST_SETTINGS).await?;
        if recipient.chosen && settings.is_none() {
            return Ok(());
        }
        let settings =
            settings.filter(|s| recipient.chosen && !recipient.withdraw && !s.endpoints.is_empty());
        let revision = settings.as_ref().map(ListSettings::revision);
        let at = key::list_reservation(&recipient.recipient);
        // Only the original batch's outcome or its atomic applied write settles it.
        // An equal readable counter can outlive an undecided batch.
        self.resolve_publications(&recipient.recipient).await?;
        let mut prior = self.local_get::<ListReservation>(&at).await?;
        let mut reason = IssueFor::Change;
        if let Some(prior) = prior.as_ref() {
            let same_choice = prior.choice == recipient.choice;
            let same_list = same_choice && prior.revision == revision;
            if same_list {
                // A raw receive can settle the conditional publication before Paykit resumes.
                self.send_message(&prior.id).await?;
            }
            let accepted = self.observe_list_acceptance(&mut recipient, prior).await?;
            if same_list && prior.confirmed && accepted.is_none() {
                // The user confirmed another version after this one's delivery ended
                // unknown (Paykit spec 8.4, 9). It is built on the counter as read now,
                // whatever another installation or a move did to it since.
                reason = IssueFor::Confirmed;
            } else if same_list {
                if prior.confirmed {
                    let mut settled = self.local_get::<ListReservation>(&at).await?;
                    if let Some(held) = settled.as_mut().filter(|held| held.matches_issue(prior)) {
                        held.confirmed = false;
                        self.local_put(&at, held).await?;
                    }
                }
                if recipient.withdraw {
                    recipient.withdraw = false;
                    self.local_put(&key::list_recipient(&recipient.recipient), &recipient)
                        .await?;
                }
                if revision.is_none() {
                    if accepted.is_some()
                        || !self
                            .service_time()
                            .is_some_and(|now| prior.withdrawal_due(now))
                    {
                        return Ok(());
                    }
                    reason = IssueFor::RetryWithdrawal;
                } else {
                    let local = ListVersion::new(prior.generation, prior.counter)?;
                    let requests = recipient.answers.has_requests();
                    let (version, missing) = if requests {
                        self.current_list_for_request(prior, accepted.is_some())
                            .await?
                    } else {
                        (local, prior.missing)
                    };
                    // A version the client gave up never blocks an answer: the newest can't
                    // go again, and the answer reconciles it (Paykit spec 8.4, 9).
                    let given_up = prior.delivery != FirstDelivery::Open;
                    if version == local && accepted.is_none() && (!requests || !given_up) {
                        return Ok(());
                    }
                    let Some(now) = self.service_time() else {
                        return Ok(());
                    };
                    let missing = if requests {
                        self.earlier_list_answers(&recipient, missing, now).await?
                    } else {
                        missing
                    };
                    match recipient.answers.decision(now, version, missing) {
                        answers::Reply::Wait => return Ok(()),
                        answers::Reply::Newer => reason = IssueFor::Answer(version),
                        answers::Reply::Repeat if version == local => {
                            return self.repeat_list(&mut recipient, prior, now, report).await
                        }
                        // The other installation queued that list in its own outbox.
                        // Its authenticated counter cannot reproduce the signed plaintext.
                        answers::Reply::Repeat => return Ok(()),
                    }
                }
            }
        }
        if !recipient.chosen
            && !recipient.withdraw
            && !matches!(reason, IssueFor::RetryWithdrawal | IssueFor::Confirmed)
        {
            return Ok(());
        }
        let endpoints = settings.map_or_else(|| Ok(BTreeMap::new()), |s| s.endpoints())?;
        if let Some(issue) = prior.as_mut() {
            if issue.delivery == FirstDelivery::Open
                && recipient.answers.first(issue).is_none()
                && self.take_prior_list_delivery(&issue.id).await?
            {
                if let Some(taken) = self.local_get::<ListReservation>(&at).await? {
                    issue.delivery = taken.delivery;
                }
            }
        }
        let issue = Box::pin(self.reserve_private_list(
            &mut recipient,
            revision,
            &endpoints,
            reason,
            prior.as_ref(),
            report,
        ))
        .await?;
        if recipient.withdraw {
            recipient.withdraw = false;
            self.local_put(&key::list_recipient(&recipient.recipient), &recipient)
                .await?;
        }
        self.send_message(&issue.msg_id).await?;
        if let Some(applied) = self
            .local_get::<ListReservation>(&at)
            .await?
            .filter(|applied| applied.matches(&issue))
        {
            self.observe_list_acceptance(&mut recipient, &applied)
                .await?;
        }
        Ok(())
    }

    async fn resolve_publications(&mut self, recipient: &[u8; 32]) -> Result<()> {
        let mut pending = false;
        for (original, ids) in self.publication_batches(recipient).await? {
            let mut remaining = PublicationIds::new();
            for &id in &ids {
                let resolved = match self.app.kept_batch(id).await? {
                    Some(KeptBatchStatus::Applied { .. }) => true,
                    Some(KeptBatchStatus::Dropped {
                        reason: KeptBatchDrop::Refused { .. } | KeptBatchDrop::Unsent,
                        ..
                    }) => {
                        // After a loss the client can sign the batch again under the same
                        // ID. A refusal of those bytes says nothing about the old service.
                        self.own_identity_confirmed(false).await?;
                        let own = self.app.own_identity();
                        own.generation() >= original.generation
                            && own
                                .body()
                                .recovery_from
                                .is_none_or(|lost| original.generation > lost)
                    }
                    _ => false,
                };
                if !resolved {
                    remaining.insert(id);
                }
            }
            pending |= !remaining.is_empty();
            if remaining != ids {
                self.retain_publication_ids(&original, &remaining).await?;
            }
        }
        if pending {
            return Err(unresolved().into());
        }
        Ok(())
    }

    /// Current version and unknown members for a new list. A withdrawal timer
    /// additionally names the exact application whose undelivered message is being replaced.
    async fn publication_base(
        &mut self,
        target: &RecordTarget,
        key: [u8; 32],
        generation: u64,
        previous: Option<&ListReservation>,
        reason: IssueFor,
        report: Option<&ReceiveReport>,
    ) -> Result<(u64, UnknownMembers, Vec<Condition>)> {
        let mut conditions = Vec::new();
        if reason == IssueFor::RetryWithdrawal {
            let prior = previous.ok_or_else(counter_unavailable)?;
            if generation < prior.generation {
                return Err(counter_unavailable().into());
            }
            let (current, condition) = self.withdrawal_base(prior).await?;
            conditions.push(condition);
            if prior.generation == generation {
                return Ok((current.counter, current.unknown, conditions));
            }
            if self.newer_list_reserved(prior, generation, report).await? {
                self.retire_obsolete_withdrawal(prior).await?;
                return Err(counter_unavailable().into());
            }
            return Ok((0, UnknownMembers::default(), conditions));
        }
        match self.app.read_record_fresh(target).await? {
            RecordRead::Fresh(held) => {
                let current = self.publication_at(&held).await?;
                if let IssueFor::Answer(expected) = reason {
                    if ListVersion::new(current.generation, current.counter)? != expected {
                        return Err(counter_unavailable().into());
                    }
                }
                Ok((current.counter, current.unknown, conditions))
            }
            RecordRead::Kept {
                record: None,
                reason: KeptReason::NotFound,
            } if self
                .local_get::<PublicationHistory>(&key::publication_history(&key))
                .await?
                .is_none()
                && previous.is_none_or(|issue| issue.generation < generation)
                && self.app.record_seen(&key).await?.is_none() =>
            {
                Ok((0, UnknownMembers::default(), conditions))
            }
            _ => Err(counter_unavailable().into()),
        }
    }

    /// Reserve a new version of a Private Payment List for its chosen recipient. The signed
    /// message enters the outbox in the same client commit as the applied counter.
    async fn reserve_private_list(
        &mut self,
        choice: &mut ListRecipient,
        revision: Option<[u8; 16]>,
        endpoints: &BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
        reason: IssueFor,
        previous: Option<&ListReservation>,
        report: Option<&ReceiveReport>,
    ) -> Result<ListPublicationRecord> {
        let recipient = choice.recipient;
        self.first_read_before_writing().await?;
        self.refresh_own()?;
        if !self.holds(SCOPE) {
            return Err(Refusal::ScopeMissing(SCOPE.into()).into());
        }
        if !endpoints.is_empty() {
            self.refuse_while_sends_held(&recipient).await?;
        }
        self.own_identity_confirmed(false).await?;
        let generation = self.app.own_identity().generation();
        let publisher = self.app_id();
        let target = target(&publisher, &recipient, generation);
        let key = self.app.record_key(&target)?;

        let (counter, unknown, conditions) = self
            .publication_base(&target, key, generation, previous, reason, report)
            .await?;
        let now = self.service_time().ok_or_else(counter_unavailable)?;
        let accepted = previous.and_then(|r| choice.answers.first(r));
        let predecessors = match previous {
            Some(prior) if prior.generation == generation && prior.counter == counter => {
                prior.payable_after(accepted)
            }
            Some(prior)
                if reason == IssueFor::RetryWithdrawal
                    && counter == 0
                    && prior.generation < generation =>
            {
                prior.payable_after(accepted)
            }
            None if counter == 0 && generation == 1 && !choice.await_repeat_time => {
                Payable::Until(0)
            }
            _ => Payable::Unknown,
        };
        if choice.await_repeat_time {
            // A repeat after the backup advances no permanent counter. This covering read
            // supplies the upper bound on the missing hour independently of fresh issues.
            choice.answers.bound_repeat(now);
            choice.restored_generation = (generation > 1).then_some((generation, now));
            choice.await_repeat_time = false;
            self.local_put(&key::list_recipient(&recipient), choice)
                .await?;
        }
        let observed = if matches!(reason, IssueFor::Answer(_)) && counter != 0 {
            self.local_get::<PublicationHistory>(&key::publication_history(&key))
                .await?
                .and_then(|history| history.missing_since)
        } else {
            None
        };
        let missing = answers::MissingAnswers::after(
            previous,
            generation,
            counter,
            observed.unwrap_or(now),
            accepted.is_some(),
        )?;
        let version = ListVersion::new(generation, counter + 1)?;
        let list = PrivatePaymentList {
            version,
            payment_endpoints: endpoints
                .iter()
                .map(|(id, payload)| {
                    (
                        id.clone(),
                        ListEndpoint {
                            payload: payload.clone(),
                            single_use: false,
                        },
                    )
                })
                .collect(),
            expires_at: self.now().saturating_add(PRIVATE_LIST_MAX_AGE_MS),
        };
        self.own_identity_confirmed(false).await?;
        if self.app.own_identity().generation() != generation || !self.holds(SCOPE) {
            return Err(counter_unavailable().into());
        }
        let signed = self.app.sign_message(OutgoingMessage::new(
            recipient,
            Kind::PrivatePaymentList.as_str(),
            list.to_body()?,
        ))?;
        let publication = ListPublicationRecord {
            publisher,
            recipient,
            generation,
            counter: counter + 1,
            msg_id: signed.msg_id().to_owned(),
            message_hash: signed.hash(),
            unknown,
        };
        let put = self
            .app
            .record_op(&target, Some(&publication.to_json()?), conditions)
            .await?;
        let at = key::list_reservation(&recipient);
        let reservation = ListReservation {
            answer: choice.answers.answer(matches!(reason, IssueFor::Answer(_))),
            missing,
            expires: list.expires_at,
            publisher,
            predecessors,
            delivery: FirstDelivery::Open,
            confirmed: false,
            withdrawal: endpoints.is_empty().then(|| Withdrawal {
                first: previous
                    .and_then(|p| p.withdrawal)
                    .filter(|_| matches!(reason, IssueFor::RetryWithdrawal | IssueFor::Confirmed))
                    .map_or(now, |w| w.first),
                issued: now,
            }),
            recipient,
            choice: choice.choice,
            revision,
            generation,
            counter: publication.counter,
            id: publication.msg_id.clone(),
            hash: publication.message_hash,
        };
        let marker = reservation.original().key();
        let mut applied = Tx::default();
        applied.put(&at, &reservation)?.delete(&marker);
        // The new version supersedes an earlier one whose delivery was unknown: its notice
        // no longer stands for a list any call reports.
        if let Some(prior) = previous {
            let notice = key::restored_delivery(&prior.recipient, &prior.id, &prior.hash);
            if self
                .kept_notice(&notice)
                .await?
                // Also when none is kept yet: the earlier version's duty can be taken while
                // this batch is pending. A notice of another kind at that key stays.
                .is_none_or(|kept| kept.kind == crate::notices::NoticeKind::ListDeliveryUnknown)
            {
                applied.delete(&notice);
            }
        }
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .entry(put)
            .queue(signed)
            .keep_id_at(marker.into_bytes())
            .locals(applied.into_writes());
        self.own_identity_confirmed(false).await?;
        if self.app.own_identity().generation() != generation || !self.holds(SCOPE) {
            return Err(counter_unavailable().into());
        }
        if matches!(reason, IssueFor::RetryWithdrawal | IssueFor::Confirmed) {
            let current = self
                .local_get::<ListRecipient>(&key::list_recipient(&recipient))
                .await?;
            if current.is_none_or(|r| r.choice != choice.choice)
                || (choice.chosen
                    && reason == IssueFor::RetryWithdrawal
                    && self
                        .local_get::<ListSettings>(key::LIST_SETTINGS)
                        .await?
                        .is_some_and(|s| !s.endpoints.is_empty()))
            {
                return Err(unresolved().into());
            }
        }
        if let Err(error) = self.submit(batch).await {
            // The client can settle the imported original from a signed conflict answer.
            if self
                .local_get::<ListReservation>(&at)
                .await?
                .is_some_and(|kept| kept.matches(&publication))
            {
                return Ok(publication);
            }
            return Err(error.into());
        }
        Ok(publication)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pubky_switch_core::ops::RecordVersion;

    fn publication() -> ListPublicationRecord {
        ListPublicationRecord {
            publisher: [1; 16],
            recipient: [2; 32],
            generation: 1,
            counter: 5,
            msg_id: "37463d05-59a1-4267-bdd5-f152b03476dc".into(),
            message_hash: [3; 32],
            unknown: UnknownMembers::default(),
        }
    }

    fn record(publication: &ListPublicationRecord, writer: [u8; 16]) -> SharedRecord {
        SharedRecord {
            key: [4; 32],
            collection: COLLECTION.into(),
            policy: target(
                &publication.publisher,
                &publication.recipient,
                publication.generation,
            )
            .collection
            .policy,
            version: RecordVersion::new(1, 2),
            authenticated: true,
            writer,
            written_at_ms: Some(10),
            content: RecordContent::Value(publication.to_json().unwrap()),
            doubtful: false,
            flagged: false,
            not_taken: false,
            different_write_at_held_version: false,
            divergent: false,
        }
    }

    #[test]
    fn test_publication_binds_the_signed_record_vectors() {
        use pubky_switch_core::enc::hash;
        use pubky_switch_core::keys::IdentityKey;
        use pubky_switch_core::ops::{OpKind, SignedOp};
        use pubky_switch_core::records::AppCert;
        use pubky_switch_core::sealed::RecordValue;
        use serde_json::Value;

        let vectors =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../specs/v0.2/vectors");
        let load = |name: &str| -> Value {
            serde_json::from_slice(&std::fs::read(vectors.join(name)).unwrap()).unwrap()
        };
        let keys = load("keys.json");
        let owner = keys["identities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "bob")
            .unwrap();
        let label = owner["seed_label"].as_str().unwrap();
        let identity =
            IdentityKey::from_secret(hash(format!("paykit/v1/test-vectors/{label}").as_bytes()));
        let naming = identity.naming_key();
        let docs = load("documents.json");
        let certificates = docs["certificates"]["cases"].as_array().unwrap();
        let sealed = load("sealed.json");
        let cases = &sealed["private_list_publications"];
        for case in cases["writes"].as_array().unwrap() {
            let cert = certificates
                .iter()
                .find(|row| row["app"] == case["writer"])
                .unwrap();
            let cert = AppCert::verify(
                &serde_json::from_value(cert["signed"].clone()).unwrap(),
                &identity.public(),
            )
            .unwrap();
            let op = SignedOp::verify_with_cert(
                &serde_json::from_value(case["operation"].clone()).unwrap(),
                &cert,
                &identity.public(),
            )
            .unwrap();
            let OpKind::RecordPut(put) = &op.body().kind else {
                panic!("expected record.put")
            };
            let scope_key = identity.scope_key(put.value_sealed.data_epoch, &put.policy.read_scope);
            let RecordValue::Json(plaintext) = put
                .value_sealed
                .open(&scope_key, &identity.public(), &put.key)
                .unwrap()
            else {
                panic!("expected a live counter");
            };
            let publication = ListPublicationRecord::read(&plaintext).unwrap();
            let held = SharedRecord {
                content: RecordContent::Value(plaintext),
                key: put.key,
                policy: put.policy.clone(),
                ..record(&publication, cert.app_id())
            };
            let result = bound_publication(&held, |target| {
                Ok(naming.record_key(
                    &target.collection.name,
                    &target.natural_key,
                    &target.collection.policy.hash().unwrap(),
                ))
            });
            assert_eq!(case["accepted"], result.is_ok(), "{}", case["name"]);
        }
    }

    #[test]
    fn test_publication_only_its_publisher_supplies_the_counter_and_floor() {
        let issued = publication();
        let own = issued.publisher;
        let sibling = [9; 16];
        let issued_bytes = issued.to_json().unwrap();
        let mark = own_counter_mark(own, &issued_bytes).unwrap();
        assert!(own_counter_mark(sibling, &issued_bytes).is_none());
        for seen in [None, Some(mark.as_slice())] {
            for counter in [2, 6, 9_007_199_254_740_991] {
                let mut observed = issued.clone();
                observed.counter = counter;
                let bytes = observed.to_json().unwrap();
                assert!(foreign_publication(Some(own), seen, Some(&bytes)).is_some());
                let refused = current_publication(&record(&observed, sibling)).unwrap_err();
                assert!(refused
                    .to_string()
                    .contains(&pubky_switch_core::enc::zbase32_encode(&sibling)));
            }
            let mut recovered = issued.clone();
            recovered.counter += 1;
            assert_eq!(
                current_publication(&record(&recovered, own)).unwrap(),
                recovered
            );
        }
        let mut other = issued;
        other.publisher = sibling;
        let bytes = other.to_json().unwrap();
        assert!(foreign_publication(Some(own), None, Some(&bytes)).is_none());
        assert!(foreign_publication(Some(own), Some(&bytes), Some(&bytes)).is_none());
        assert!(foreign_publication(Some(own), Some(&mark), Some(&bytes)).is_some());
    }

    #[test]
    fn test_publication_discovery_and_allocation_require_a_current_trusted_write() {
        let issued = publication();
        let held = record(&issued, issued.publisher);
        for invalidate in [
            |r: &mut SharedRecord| r.authenticated = false,
            |r: &mut SharedRecord| r.flagged = true,
            |r: &mut SharedRecord| r.not_taken = true,
            |r: &mut SharedRecord| r.different_write_at_held_version = true,
            |r: &mut SharedRecord| r.divergent = true,
            |r: &mut SharedRecord| r.doubtful = true,
        ] {
            let mut uncertain = held.clone();
            invalidate(&mut uncertain);
            assert!(current_publication(&uncertain).is_err());
        }
        assert_eq!(current_publication(&held).unwrap(), issued);
    }

    #[test]
    fn test_publication_restore_keeps_the_greatest_applied_floor_without_renewing_consent() {
        let issued = publication();
        let at = key::list_reservation(&issued.recipient);
        let binder = state::Binder::unblinded([8; 32]);
        let issue = |counter, hash| ListReservation {
            answer: Default::default(),
            missing: None,
            expires: 1000,
            publisher: issued.publisher,
            predecessors: Payable::Until(0),
            delivery: FirstDelivery::Open,
            confirmed: false,
            withdrawal: None,
            recipient: issued.recipient,
            choice: [7; 16],
            revision: Some([6; 16]),
            generation: issued.generation,
            counter,
            id: issued.msg_id.clone(),
            hash: [hash; 32],
        };
        for pair in [[(3, 4), (6, 5)], [(6, 5), (3, 4)]] {
            let mut restored = RestoredPublications::default();
            for (counter, hash) in pair {
                restored
                    .applied(
                        at.as_bytes(),
                        &state::encode(&issue(counter, hash)).unwrap(),
                        &binder,
                    )
                    .unwrap();
            }
            let mut floor = None;
            let mut choice = None;
            for write in restored.writes().unwrap() {
                if let LocalWrite::Put { key: k, value } = write {
                    if k == at.as_bytes() {
                        floor =
                            Some(state::restored::<ListReservation>(&k, &value, &binder).unwrap());
                    } else if k == key::list_recipient(&issued.recipient).as_bytes() {
                        choice =
                            Some(state::restored::<ListRecipient>(&k, &value, &binder).unwrap());
                    }
                }
            }
            let floor = floor.unwrap();
            let choice = choice.unwrap();
            assert_eq!(
                (floor.generation, floor.counter, floor.hash),
                (1, 6, [5; 32])
            );
            assert!(!choice.chosen && choice.withdraw);
            assert_ne!(choice.choice, floor.choice);
        }
        let mut restored = RestoredPublications::default();
        restored
            .applied(
                at.as_bytes(),
                &state::encode(&issue(5, 4)).unwrap(),
                &binder,
            )
            .unwrap();
        assert!(restored
            .applied(
                at.as_bytes(),
                &state::encode(&issue(5, 5)).unwrap(),
                &binder
            )
            .is_err());
    }
}
