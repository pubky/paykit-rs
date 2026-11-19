//! The canonical Allowance view and the conditional discard of a pending conflict.

use super::{
    allowance_record_target, allowance_target, AllowanceStatus, AllowanceView, Paykit,
    PendingConflict,
};
use crate::error::{PaykitError, Refusal, Result};
use crate::state::key;
use paykit_lib::collections::Collection;
use paykit_lib::records::PendingAllowanceRecord;
use pubky_switch_client::app::{
    KeptReason, OpBatch, RecordContent, RecordRead, RecordTarget, SharedRecord,
};
use pubky_switch_core::ops::{Condition, RecordVersion};
use std::collections::BTreeMap;

fn not_current() -> PaykitError {
    Refusal::NotNow("the current pending proposal and Allowance could not be verified".into())
        .into()
}

impl Paykit {
    fn selected_pending(
        &self,
        held: &SharedRecord,
    ) -> Result<(PendingAllowanceRecord, RecordTarget)> {
        let own = self.identity();
        let value = held.content.value().ok_or_else(not_current)?;
        let pending = PendingAllowanceRecord::read(value, &own)?;
        let target = allowance_record_target(
            Collection::AllowanceProposals,
            pending.counterparty(),
            pending.allowance_id(),
        );
        let key = self.app.record_key(&target)?;
        if !super::replies::qualified_at(held, &target, &key) {
            return Err(not_current());
        }
        Ok((pending, target))
    }

    /// Locate the pending key from the same canonical Allowance key every wallet method accepts.
    async fn pending_by_allowance_key(
        &self,
        record: &[u8; 32],
    ) -> Result<Option<(SharedRecord, RecordTarget)>> {
        let mut unresolved = false;
        for held in self
            .app
            .records_of(Collection::AllowanceProposals.name())
            .await?
        {
            if matches!(held.content, RecordContent::Deleted) {
                continue;
            }
            let Ok((pending, target)) = self.selected_pending(&held) else {
                unresolved = true;
                continue;
            };
            if self.app.record_key(&allowance_target(
                pending.counterparty(),
                pending.allowance_id(),
            ))? == *record
            {
                return Ok(Some((held, target)));
            }
        }
        if unresolved {
            return Err(not_current());
        }
        Ok(None)
    }

    /// A retained local stop still shows the proposed terms when the shared value
    /// is unreadable. Only the verified end of its lifetime retires that stop.
    pub(super) async fn pending_conflicts(
        &self,
        wanted: Option<&[u8; 32]>,
    ) -> Result<Vec<AllowanceView>> {
        let mut by_key = BTreeMap::new();
        for (_, stop) in self
            .local_scan::<PendingConflict>(key::PENDING_CONFLICTS)
            .await?
        {
            let pending = stop.original(&self.identity())?;
            let target = allowance_record_target(
                Collection::AllowanceProposals,
                pending.counterparty(),
                pending.allowance_id(),
            );
            by_key.insert(self.app.record_key(&target)?, pending);
        }
        let mut unresolved = false;
        for held in self
            .app
            .records_of(Collection::AllowanceProposals.name())
            .await?
        {
            if by_key.contains_key(&held.key) || matches!(held.content, RecordContent::Deleted) {
                continue;
            }
            match self.selected_pending(&held) {
                Ok((pending, _)) if pending.conflicted().is_some() => {
                    by_key.insert(held.key, pending);
                }
                Ok(_) => (),
                Err(_) => unresolved = true,
            }
        }
        let mut out = Vec::new();
        for pending in by_key.into_values() {
            let permanent = self.app.record_key(&allowance_target(
                pending.counterparty(),
                pending.allowance_id(),
            ))?;
            if wanted.is_some_and(|key| *key != permanent)
                || self.app.shared_record(&permanent).await?.is_some()
            {
                continue;
            }
            out.push(
                Box::pin(self.proposal_view(
                    permanent,
                    pending.proposal(),
                    AllowanceStatus::Conflicted,
                ))
                .await?,
            );
        }
        if unresolved && (wanted.is_none() || out.is_empty()) {
            return Err(not_current());
        }
        Ok(out)
    }

    /// Discard a conflicted pending proposal or one received from a blocked counterparty.
    /// This removes exactly the selected pending write and sends no message (Paykit spec 8.3.1).
    ///
    /// # Errors
    ///
    /// A changed proposal or an uncertain read leaves the pending record in place.
    pub async fn discard_pending_allowance(&mut self, record: &[u8; 32]) -> Result<()> {
        self.refresh_own()?;
        self.require_allowance()?;
        let (selected, target) = Box::pin(self.pending_by_allowance_key(record))
            .await?
            .ok_or(Refusal::NotFound)?;
        let condition = self.app.record_condition(&selected.key).await?;
        self.first_read_before_writing().await?;
        self.refresh_own()?;
        self.require_allowance()?;
        self.records_known().await?;
        let RecordRead::Fresh(held) = self.app.read_record_fresh(&target).await? else {
            return Err(not_current());
        };
        let (current, _) = self.selected_pending(&held)?;
        if held.content != selected.content
            || self.app.record_condition(&held.key).await? != condition
        {
            return Err(not_current());
        }
        let peer = current.counterparty();
        let a = allowance_target(peer, current.allowance_id());
        if !matches!(
            self.app.read_record_fresh(&a).await?,
            RecordRead::Kept {
                reason: KeptReason::NotFound,
                record: None
            }
        ) {
            return Err(not_current());
        }
        let stop = self
            .local_get::<PendingConflict>(&key::pending_conflict(&held.key))
            .await?;
        let local_conflict = stop
            .as_ref()
            .map(|s| s.stops(&self.identity(), &current))
            .transpose()?
            .unwrap_or(false);
        if current.conflicted().is_none()
            && !local_conflict
            && (current.proposal().sender() != *peer || !self.denied(peer).await?)
        {
            return Err(Refusal::Usage("only a conflicted proposal or a blocked counterparty's received proposal may be discarded".into()).into());
        }
        let conditions = vec![
            condition,
            Condition::RecordVersion {
                key: *record,
                version: RecordVersion::NONE,
            },
        ];
        let delete = self.app.record_op(&target, None, conditions).await?;
        self.submit(OpBatch::for_protocol(crate::protocol::BACKUP_NAME).entry(delete))
            .await?;
        Ok(())
    }
}
