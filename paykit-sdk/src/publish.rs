//! Publishing a Private Payment List to a chosen counterparty (Paykit spec 8.4.1).

use std::collections::BTreeMap;

use paykit_lib::bodies::{ListEndpoint, PrivatePaymentList, PRIVATE_LIST_MAX_AGE_MS};
use paykit_lib::derive::natural_key;
use paykit_lib::records::{ListPublicationRecord, UnknownMembers};
use paykit_lib::{Kind, ListVersion, PaymentEndpointIdentifier, PaymentEndpointPayload};
use pubky_switch_client::app::{
    AppError, Collection, DeliveryView, KeptBatchDrop, KeptBatchStatus, KeptReason, OpBatch,
    OutgoingMessage, RecordContent, RecordRead, RecordTarget, SharedRecord, StoreFailure,
};
use pubky_switch_core::ident::Policy;

use crate::error::{Refusal, Result};
use crate::state::{key, Tx};
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
}

impl ListRecipient {
    fn new(recipient: [u8; 32], chosen: bool) -> Result<Self> {
        Ok(Self {
            recipient,
            chosen,
            choice: random_bytes()?,
            withdraw: !chosen,
        })
    }

    pub(crate) fn restored(mut self) -> Result<Self> {
        self.choice = random_bytes()?;
        self.withdraw = true;
        Ok(self)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListReservation {
    pub(crate) recipient: [u8; 32],
    choice: [u8; 16],
    #[serde(deserialize_with = "Option::deserialize")]
    revision: Option<[u8; 16]>,
    generation: u64,
    counter: u64,
    id: String,
    hash: [u8; 32],
    applied: bool,
    #[serde(deserialize_with = "Option::deserialize")]
    batch_id: Option<[u8; 16]>,
}

impl ListReservation {
    pub(crate) fn check(&self) -> Result<()> {
        ListVersion::new(self.generation, self.counter)?;
        pubky_switch_core::json::check_uuid_v4(&self.id)?;
        Ok(())
    }

    fn matches(&self, record: &ListPublicationRecord) -> bool {
        self.id == record.msg_id
            && self.hash == record.message_hash
            && self.generation == record.generation
            && self.counter == record.counter
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
        if !issue.applied
            || issue.choice != recipient.choice
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

impl Paykit {
    fn bound_publication(&self, record: &SharedRecord) -> Result<ListPublicationRecord> {
        let bytes = record.content.value().ok_or_else(counter_unavailable)?;
        let publication = ListPublicationRecord::read(bytes)?;
        let expected = target(
            &publication.publisher,
            &publication.recipient,
            publication.generation,
        );
        if !record.authenticated
            || record.collection != COLLECTION
            || record.policy != expected.collection.policy
            || self.app.record_key(&expected)? != record.key
        {
            return Err(counter_unavailable().into());
        }
        Ok(publication)
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
            let chosen = ListRecipient::new(recipient, true)?;
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
            let left = ListRecipient::new(recipient, false)?;
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
                let Ok(prior) = self.bound_publication(&held) else {
                    // A later key or a readable successor can still identify its recipient.
                    undiscovered |= held.content != RecordContent::Deleted;
                    continue;
                };
                if prior.publisher != self.app_id() {
                    continue;
                }
                let at = key::list_recipient(&prior.recipient);
                if self.local_get::<ListRecipient>(&at).await?.is_none() {
                    self.local_put(&at, &ListRecipient::new(prior.recipient, false)?)
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
        if !recipient.withdraw && (!recipient.chosen || settings.is_none()) {
            return Ok(());
        }
        let settings = settings.filter(|s| !recipient.withdraw && !s.endpoints.is_empty());
        let revision = settings.as_ref().map(|s| s.revision);
        let at = key::list_reservation(&recipient.recipient);
        if let Some(mut prior) = self.local_get::<ListReservation>(&at).await? {
            if !prior.applied {
                let fate = match prior.batch_id {
                    Some(id) => self.app.kept_batch(id).await?,
                    None => None,
                };
                match fate {
                    Some(KeptBatchStatus::Applied { .. }) => {
                        prior.applied = true;
                        self.local_put(&at, &prior).await?;
                    }
                    Some(KeptBatchStatus::Dropped {
                        reason: KeptBatchDrop::Refused { .. } | KeptBatchDrop::Unsent,
                        ..
                    }) => self.local_delete(&at).await?,
                    // The same counter value can survive an undecided batch. Only the
                    // original batch's outcome or its atomic local write settles it.
                    _ => return Err(unresolved().into()),
                }
            }
            if prior.applied && prior.choice == recipient.choice && prior.revision == revision {
                if recipient.withdraw {
                    recipient.withdraw = false;
                    self.local_put(&key::list_recipient(&recipient.recipient), &recipient)
                        .await?;
                }
                return Ok(());
            }
        }
        let endpoints = settings.map_or_else(|| Ok(BTreeMap::new()), |s| s.endpoints())?;
        let issue = Box::pin(self.reserve_private_list(&recipient, revision, &endpoints)).await?;
        if recipient.withdraw {
            recipient.withdraw = false;
            self.local_put(&key::list_recipient(&recipient.recipient), &recipient)
                .await?;
        }
        self.send_message(&issue.msg_id).await?;
        Ok(())
    }

    /// Reserve a new version of a Private Payment List for its chosen recipient. The signed
    /// message enters the outbox in the same client commit as the applied counter.
    async fn reserve_private_list(
        &mut self,
        choice: &ListRecipient,
        revision: Option<[u8; 16]>,
        endpoints: &BTreeMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
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
        let seen = self
            .app
            .record_seen(&key)
            .await?
            .as_deref()
            .map(ListPublicationRecord::read)
            .transpose()?;
        let (counter, unknown) = match self.app.read_record_fresh(&target).await? {
            RecordRead::Fresh(held)
                if !held.flagged
                    && !held.different_write_at_held_version
                    && !held.divergent
                    && !held.doubtful =>
            {
                let current = self.bound_publication(&held)?;
                (current.counter, current.unknown)
            }
            RecordRead::Kept {
                record: None,
                reason: KeptReason::NotFound,
            } if seen.is_none() => (0, UnknownMembers::default()),
            _ => return Err(counter_unavailable().into()),
        };
        if seen.as_ref().is_some_and(|floor| {
            floor.publisher != publisher
                || floor.recipient != recipient
                || floor.generation != generation
        }) {
            return Err(counter_unavailable().into());
        }
        let counter = counter.max(seen.map_or(0, |floor| floor.counter));
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
        let mut reservation = ListReservation {
            recipient,
            choice: choice.choice,
            revision,
            generation,
            counter: publication.counter,
            id: publication.msg_id.clone(),
            hash: publication.message_hash,
            applied: false,
            batch_id: None,
        };
        reservation.applied = true;
        let mut applied = Tx::default();
        applied.put(&at, &reservation)?;
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .entry(put)
            .queue(signed)
            .locals(applied.into_writes());
        self.own_identity_confirmed(false).await?;
        if self.app.own_identity().generation() != generation || !self.holds(SCOPE) {
            return Err(counter_unavailable().into());
        }
        reservation.applied = false;
        self.local_put(&at, &reservation).await?;
        if let Err(error) = self.submit(batch).await {
            // The client can settle the imported original from a signed conflict answer.
            if self
                .local_get::<ListReservation>(&at)
                .await?
                .is_some_and(|kept| kept.applied && kept.matches(&publication))
            {
                return Ok(publication);
            }
            if let Some(id) = error.kept_batch() {
                reservation.applied = false;
                reservation.batch_id = Some(id);
                self.local_put(&at, &reservation).await?;
            } else if !matches!(error, AppError::Store(_) | AppError::Protocol(_)) {
                // A bare submit refusal kept nothing. A store failure can hide a kept batch.
                self.local_delete(&at).await?;
            }
            return Err(error.into());
        }
        Ok(publication)
    }
}
