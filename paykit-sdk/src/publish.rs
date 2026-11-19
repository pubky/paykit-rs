//! Publishing a Private Payment List to a chosen counterparty (Paykit spec 8.4.1).
//!
//! A native batch keeps the original message’s hash, recipient and generation in a
//! protocol-local key naming the batch. Its application records the sharing decision
//! and queues the signed message together. If the application is undecided, changes
//! for that recipient wait while other recipients can proceed. An in-place restore
//! retains outstanding originals from both stores and requires a fresh sharing choice.
//! Development stores containing the earlier separate reservation fail validation. An
//! unresolved reservation from that format cannot establish whether the batch was kept.

pub(crate) mod answers;
pub(crate) mod history;

use history::PublicationHistory;
use std::collections::{BTreeMap, BTreeSet};

use paykit_lib::bodies::{ListEndpoint, PrivatePaymentList, PRIVATE_LIST_MAX_AGE_MS};
use paykit_lib::derive::natural_key;
use paykit_lib::records::{ListPublicationRecord, UnknownMembers};
use paykit_lib::{Kind, ListVersion, PaymentEndpointIdentifier, PaymentEndpointPayload};
use pubky_switch_client::app::{
    Collection, DeliveryView, KeptBatchDrop, KeptBatchStatus, KeptReason, LocalWrite, OpBatch,
    OutgoingMessage, RecordContent, RecordRead, RecordTarget, SharedRecord, StoreFailure,
};
use pubky_switch_core::ident::Policy;

use crate::error::{Refusal, Result};
use crate::state::{self, key, PublicationIds, PublicationKey, Tx};
use crate::Paykit;
use pubky_switch_core::enc::random_bytes;
use serde::{Deserialize, Serialize};

pub(crate) const COLLECTION: &str = "private_list_publications";
const SCOPE: &str = "paykit_request";

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

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListSettings {
    revision: [u8; 16],
    endpoints: BTreeMap<String, String>,
}

impl ListSettings {
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

    fn matches(&self, record: &ListPublicationRecord) -> bool {
        self.id == record.msg_id
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
        let issue = state::restored::<ListReservation>(key, value, binder)?;
        self.remember_issue(
            issue.recipient,
            issue.generation,
            issue.counter,
            &issue.id,
            issue.hash,
        )?;
        self.recipients.insert(issue.recipient);
        self.applied.insert(issue.original());
        if let Some(held) = self.floors.get(&issue.recipient) {
            let prior = (held.generation, held.counter);
            let next = (issue.generation, issue.counter);
            if next <= prior {
                return Ok(());
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
            || !settings.is_some_and(|current| Some(current.revision) == issue.revision)
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
        self.local_put(
            key::LIST_SETTINGS,
            &ListSettings {
                revision: random_bytes()?,
                endpoints,
            },
        )
        .await?;
        Box::pin(self.tend_private_lists()).await
    }

    /// Share the current reusable list with the counterparty the host chose.
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
        let mut state = self.local_get::<ListRecipient>(&at).await?;
        if state.as_ref().is_none_or(|r| !r.chosen) {
            let chosen = ListRecipient::choose(state, recipient, true)?;
            self.local_put(&at, &chosen).await?;
            state = Some(chosen);
        }
        if let Some(chosen) = state {
            Box::pin(self.publish_recipient(chosen)).await?;
        }
        Ok(())
    }

    /// Stop sharing with this counterparty. A fresh, empty list supersedes what it may hold.
    pub async fn withdraw_list_from(&mut self, recipient: [u8; 32]) -> Result<()> {
        self.first_read_before_writing().await?;
        let at = key::list_recipient(&recipient);
        let Some(prior) = self.local_get::<ListRecipient>(&at).await? else {
            return Ok(());
        };
        if prior.chosen {
            let left = ListRecipient::choose(Some(prior), recipient, false)?;
            self.local_put(&at, &left).await?;
            Box::pin(self.publish_recipient(left)).await?;
        } else {
            Box::pin(self.publish_recipient(prior)).await?;
        }
        Ok(())
    }

    pub(crate) async fn tend_private_lists(&mut self) -> Result<()> {
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
            if let Err(error) = Box::pin(self.publish_recipient(recipient)).await {
                if crate::hold::local_failure(&error) {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    async fn publish_recipient(&mut self, mut recipient: ListRecipient) -> Result<()> {
        let settings = self.local_get::<ListSettings>(key::LIST_SETTINGS).await?;
        if recipient.chosen && settings.is_none() {
            return Ok(());
        }
        let settings =
            settings.filter(|s| recipient.chosen && !recipient.withdraw && !s.endpoints.is_empty());
        let revision = settings.as_ref().map(|s| s.revision);
        let at = key::list_reservation(&recipient.recipient);
        // Only the original batch's outcome or its atomic applied write settles it.
        // An equal readable counter can outlive an undecided batch.
        self.resolve_publications(&recipient.recipient).await?;
        let prior = self.local_get::<ListReservation>(&at).await?;
        let mut requested = false;
        if let Some(prior) = prior.as_ref() {
            let same_choice = prior.choice == recipient.choice;
            let same_list = same_choice && prior.revision == revision;
            if same_list {
                // A raw receive can settle the conditional publication before Paykit resumes.
                self.send_message(&prior.id).await?;
            }
            let accepted = self.observe_list_acceptance(&mut recipient, prior).await?;
            if same_list {
                if recipient.withdraw {
                    recipient.withdraw = false;
                    self.local_put(&key::list_recipient(&recipient.recipient), &recipient)
                        .await?;
                }
                let Some(now) = self.service_time() else {
                    return Ok(());
                };
                if !recipient.chosen || revision.is_none() || accepted.is_none() {
                    return Ok(());
                }
                let version = ListVersion::new(prior.generation, prior.counter)?;
                let missing = if recipient.answers.has_requests() {
                    self.earlier_list_answers(&recipient, prior.missing, now)
                        .await?
                } else {
                    prior.missing
                };
                match recipient.answers.decision(now, version, missing) {
                    answers::Reply::Wait => return Ok(()),
                    answers::Reply::Newer => requested = true,
                    answers::Reply::Repeat => {
                        return self.repeat_list(&mut recipient, prior, now).await
                    }
                }
            }
        }
        if !recipient.chosen && !recipient.withdraw {
            return Ok(());
        }
        let endpoints = settings.map_or_else(|| Ok(BTreeMap::new()), |s| s.endpoints())?;
        let issue =
            Box::pin(self.reserve_private_list(&mut recipient, revision, &endpoints, requested))
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

    /// Reserve a new version of a Private Payment List for its chosen recipient. The signed
    /// message enters the outbox in the same client commit as the applied counter.
    async fn reserve_private_list(
        &mut self,
        choice: &mut ListRecipient,
        revision: Option<[u8; 16]>,
        endpoints: &BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
        requested: bool,
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

        let (counter, unknown) = match self.app.read_record_fresh(&target).await? {
            RecordRead::Fresh(held) => {
                let current = self.publication_at(&held).await?;
                (current.counter, current.unknown)
            }
            RecordRead::Kept {
                record: None,
                reason: KeptReason::NotFound,
            } if self
                .local_get::<PublicationHistory>(&key::publication_history(&key))
                .await?
                .is_none()
                && self
                    .local_get::<ListReservation>(&key::list_reservation(&recipient))
                    .await?
                    .is_none_or(|issue| issue.generation != generation)
                && self.app.record_seen(&key).await?.is_none() =>
            {
                (0, UnknownMembers::default())
            }
            _ => return Err(counter_unavailable().into()),
        };
        let previous = self
            .local_get::<ListReservation>(&key::list_reservation(&recipient))
            .await?;
        let now = self.service_time().ok_or_else(counter_unavailable)?;
        if choice.await_repeat_time {
            // A repeat after the backup advances no permanent counter. This covering read
            // supplies the upper bound on the missing hour independently of fresh issues.
            choice.answers.bound_repeat(now);
            choice.restored_generation = (generation > 1).then_some((generation, now));
            choice.await_repeat_time = false;
            self.local_put(&key::list_recipient(&recipient), choice)
                .await?;
        }
        let accepted = previous
            .as_ref()
            .is_some_and(|r| choice.answers.first(r).is_some());
        let missing =
            answers::MissingAnswers::after(previous.as_ref(), generation, counter, now, accepted)?;
        if requested
            && previous
                .as_ref()
                .is_none_or(|r| r.generation != generation || r.counter != counter)
        {
            // This installation has no accepted signed original for the newly visible counter.
            return Err(counter_unavailable().into());
        }
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
            .record_op(&target, Some(&publication.to_json()?), vec![])
            .await?;
        let at = key::list_reservation(&recipient);
        let reservation = ListReservation {
            answer: choice.answers.answer(requested),
            missing,
            expires: list.expires_at,
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
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .entry(put)
            .queue(signed)
            .keep_id_at(marker.into_bytes())
            .locals(applied.into_writes());
        self.own_identity_confirmed(false).await?;
        if self.app.own_identity().generation() != generation || !self.holds(SCOPE) {
            return Err(counter_unavailable().into());
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
