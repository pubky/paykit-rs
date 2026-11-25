//! Evidence that may still establish a conflict after ordinary placement expires.

use super::{
    allowance_record_target, bind_at, conflicts, named_allowance, placed_with_allowance, replies,
    AllowanceRecord, Paykit,
};
use crate::divergent::SeenVersion;
use crate::error::{PaykitError, Result};
use crate::notices::{GivenUpFrom, GivenUpOrigin, KeptNotice};
use crate::receive::{self, Fail};
use crate::records::KeptMessage;
use crate::state::{self, key, Binder};
use paykit_lib::{collections::Collection, records::StoredMessage, Body};
use pubky_dispatch_client::app::{
    DeliveryView, FeedPosition, LocalWrite, ReceiveContext, SharedRecord,
};
use serde::{Deserialize, Serialize};

/// One authenticated representative per physical record. A different input that
/// cannot be retained beside it keeps the record held even if the representative
/// later proves harmless. A pending hold belongs only to its observed lifetime.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConflictEvidence {
    message: KeptMessage,
    pending: Option<SeenVersion>,
    additional: bool,
    /// Two distinct originals were observed at the selected pending version.
    /// This proves a local discard is permitted even when the record holds our representative.
    pending_pair: bool,
}

impl ConflictEvidence {
    pub(crate) fn bind(&self, record: &[u8; 32], binder: &Binder<'_>) -> Result<()> {
        let message = self.message.stored(binder.identity())?;
        self.message.received_at()?;
        if (self.pending_pair && (self.pending.is_none() || !self.additional))
            || self.message.pending_create.is_some()
            || self.message.ended_acceptance.is_some()
        {
            return Err(PaykitError::corrupt());
        }
        let id = named_allowance(message.body()).ok_or_else(PaykitError::corrupt)?;
        let family = if let Some(source) = self.pending {
            if source.record_version().is_none()
                || !matches!(message.body(), Body::AllowanceProposal(_))
            {
                return Err(PaykitError::corrupt());
            }
            Collection::AllowanceProposals
        } else {
            if !placed_with_allowance(message.body()) {
                return Err(PaykitError::corrupt());
            }
            Collection::Allowances
        };
        if binder.record_key(&allowance_record_target(family, &message.sender(), id))? != *record {
            return Err(PaykitError::corrupt());
        }
        Ok(())
    }

    pub(super) fn retired_by(&self, end: &replies::PendingRetirement) -> bool {
        self.pending.is_some_and(|source| end.retires(source))
    }

    pub(super) fn merge(&mut self, other: &Self, own: &[u8; 32]) -> Result<()> {
        if self.pending.is_some() != other.pending.is_some() {
            return Err(PaykitError::corrupt());
        }
        let distinct = self.message.stored(own)?.hash() != other.message.stored(own)?.hash();
        self.additional |= other.additional || self.pending != other.pending || distinct;
        if self.pending.is_some() && self.pending == other.pending {
            self.pending_pair |= other.pending_pair || distinct;
        }
        if other.pending > self.pending {
            self.pending = other.pending;
            self.pending_pair = other.pending_pair;
            self.message = other.message.clone();
        }
        Ok(())
    }

    fn writes(
        existing: Option<Self>,
        own: &[u8; 32],
        record: &[u8; 32],
        message: &StoredMessage,
        evidence: &KeptMessage,
        pending: Option<SeenVersion>,
        now: u64,
    ) -> Result<Vec<LocalWrite>> {
        let mut next = Self {
            message: evidence.clone(),
            pending,
            additional: false,
            pending_pair: false,
        };
        let mut writes = Vec::new();
        if let Some(mut held) = existing {
            held.merge(&next, own)?;
            next = held;
        } else {
            let sender = message.sender();
            let id = &message.message().msg_id;
            let notice = KeptNotice::given_up(
                sender,
                id,
                None,
                now,
                GivenUpFrom::Origin(GivenUpOrigin::AllowanceConflict),
            );
            writes.push(state::local_put(
                &key::message_notice(&sender, id),
                &notice,
            )?);
        }
        writes.push(state::local_put(&key::allowance_evidence(record), &next)?);
        Ok(writes)
    }
}

pub(super) fn retain(
    cx: &mut ReceiveContext<'_>,
    stored: &StoredMessage,
    position: FeedPosition,
    record: &[u8; 32],
    pending: Option<SeenVersion>,
) -> std::result::Result<(), Fail> {
    let existing = state::cx_get::<ConflictEvidence>(cx, &key::allowance_evidence(record))?;
    let writes = ConflictEvidence::writes(
        existing,
        &cx.identity(),
        record,
        stored,
        &KeptMessage::taken_at(stored, position),
        pending,
        cx.highest_service_time().ok_or_else(Fail::corrupt)?,
    )
    .map_err(|_| Fail::corrupt())?;
    for write in writes {
        receive::keep_local(cx, write);
    }
    Ok(())
}

pub(super) fn holds_delivery(
    view: &DeliveryView<'_>,
    pending: &[u8; 32],
    permanent: &[u8; 32],
) -> Result<bool> {
    for record in [pending, permanent] {
        if state::delivery_get::<ConflictEvidence>(view, &key::allowance_evidence(record))?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Paykit {
    pub(crate) async fn unresolved_allowance(&self, record: &[u8; 32]) -> Result<bool> {
        Ok(self
            .local_get::<ConflictEvidence>(&key::allowance_evidence(record))
            .await?
            .is_some())
    }

    pub(crate) async fn unresolved_for_allowance(
        &self,
        record: &[u8; 32],
        bound: &AllowanceRecord,
    ) -> Result<bool> {
        let pending = allowance_record_target(
            Collection::AllowanceProposals,
            &bound.counterparty,
            &bound.allowance_id,
        );
        Ok(self.unresolved_allowance(record).await?
            || self
                .unresolved_allowance(&self.app.record_key(&pending)?)
                .await?)
    }

    /// Transfer custody before the kept or stored entry is released.
    pub(super) async fn expire_allowance_placement(
        &mut self,
        record: &[u8; 32],
        pending: Option<SeenVersion>,
        message: &StoredMessage,
        receipt: &KeptMessage,
        mut locals: Vec<LocalWrite>,
    ) -> Result<super::Placed> {
        let accepted = receipt.received_at()?.0;
        if !crate::stored::past_window(accepted, self.service_time()) {
            return Ok(super::Placed::Not);
        }
        let existing = self
            .local_get::<ConflictEvidence>(&key::allowance_evidence(record))
            .await?;
        locals.extend(ConflictEvidence::writes(
            existing,
            &self.identity(),
            record,
            message,
            receipt,
            pending,
            self.service_time().ok_or_else(PaykitError::corrupt)?,
        )?);
        self.app
            .protocol_commit(crate::protocol::BACKUP_NAME, &locals)
            .await?;
        Ok(super::Placed::Done)
    }

    /// Resolve only from a qualified live record. A real permanent conflict or
    /// lifetime-bound pending stop takes custody before the uncertainty clears.
    pub(super) async fn resolve_allowance_evidence(&mut self) -> Result<()> {
        let own = self.identity();
        for (suffix, evidence) in self
            .local_scan::<ConflictEvidence>(key::ALLOWANCE_EVIDENCE)
            .await?
        {
            let at = format!("{}{suffix}", key::ALLOWANCE_EVIDENCE);
            let message = evidence.message.stored(&own)?;
            let id = named_allowance(message.body()).ok_or_else(PaykitError::corrupt)?;
            let peer = message.sender();
            let family = if evidence.pending.is_some() {
                Collection::AllowanceProposals
            } else {
                Collection::Allowances
            };
            let target = allowance_record_target(family, &peer, id);
            let record = self.app.record_key(&target)?;
            let Some(held) = self.app.shared_record(&record).await? else {
                continue;
            };
            if !replies::qualified_at(&held, &target, &record) {
                continue;
            }
            if evidence.pending.is_some() {
                self.resolve_pending_evidence(&held, &target, &record, &evidence, &message)
                    .await?;
                continue;
            }
            let Some(bound) = held
                .content
                .value()
                .and_then(|v| bind_at(v, &own, &peer, id))
            else {
                continue;
            };
            if bound.conflicted.is_none() && conflicts(&bound, &message) {
                let release = LocalWrite::Delete {
                    key: at.into_bytes(),
                };
                self.keep_conflict_later(&record, &message, &evidence.message, vec![release])
                    .await?;
                continue;
            }
            // A matching answer without the original response still cannot be compared.
            if bound.conflicted.is_none()
                && (evidence.additional
                    || (super::is_allowance_response(message.body())
                        && message.sender() == bound.proposal.message().to
                        && !super::misreferenced(&bound, &message)
                        && bound.response.is_none()))
            {
                continue;
            }
            self.local_delete(&at).await?;
        }
        Ok(())
    }

    /// A retained input alone cannot authorize deleting an unrelated live proposal.
    pub(super) async fn may_discard_with_evidence(
        &self,
        target: &pubky_dispatch_client::app::RecordTarget,
        held: &SharedRecord,
        current: &paykit_lib::records::PendingAllowanceRecord,
    ) -> Result<bool> {
        let Some(evidence) = self
            .local_get::<ConflictEvidence>(&key::allowance_evidence(&held.key))
            .await?
        else {
            return Ok(false);
        };
        let Some(source) = evidence.pending else {
            return Err(PaykitError::corrupt());
        };
        let candidate = evidence.message.stored(&self.identity())?;
        Ok(
            (evidence.pending_pair || candidate.hash() != current.proposal().hash())
                && self.same_pending_lifetime(target, held, source).await?,
        )
    }

    async fn same_pending_lifetime(
        &self,
        target: &pubky_dispatch_client::app::RecordTarget,
        held: &SharedRecord,
        source: SeenVersion,
    ) -> Result<bool> {
        let source = source.record_version();
        Ok(held.version == source || self.pending_predecessor(target, held).await? == Some(source))
    }

    async fn resolve_pending_evidence(
        &mut self,
        held: &SharedRecord,
        target: &pubky_dispatch_client::app::RecordTarget,
        record: &[u8; 32],
        evidence: &ConflictEvidence,
        message: &StoredMessage,
    ) -> Result<()> {
        let own = self.identity();
        let Some(p) = replies::qualified(held, target, record, &own) else {
            return Ok(());
        };
        if p.counterparty() != &message.sender()
            || Some(p.allowance_id()) != named_allowance(message.body())
        {
            return Ok(());
        }
        let source = evidence.pending.ok_or_else(PaykitError::corrupt)?;
        if !self.same_pending_lifetime(target, held, source).await? {
            return Ok(());
        }
        if p.conflicted().is_some() {
            return self.local_delete(&key::allowance_evidence(record)).await;
        }
        // A later record holding the candidate as its only original cannot establish
        // what the unreadable earlier version held.
        if held.version != source.record_version() && p.proposal().hash() == message.hash() {
            return Ok(());
        }
        let key = key::pending_conflict(record);
        let stop = self.local_get::<super::PendingConflict>(&key).await?;
        match replies::second_proposal(
            &own,
            held,
            target,
            record,
            stop,
            message,
            evidence
                .message
                .position()
                .ok_or_else(PaykitError::corrupt)?,
        )? {
            replies::SecondProposal::Reflected if !evidence.additional => {
                self.local_delete(&key::allowance_evidence(record)).await
            }
            replies::SecondProposal::Reflected | replies::SecondProposal::Undecided => Ok(()),
            replies::SecondProposal::Keep(stop) => {
                let release = LocalWrite::Delete {
                    key: key::allowance_evidence(record).into_bytes(),
                };
                self.app
                    .protocol_commit(
                        crate::protocol::BACKUP_NAME,
                        &[state::local_put(&key, &*stop)?, release],
                    )
                    .await?;
                Ok(())
            }
        }
    }
}
