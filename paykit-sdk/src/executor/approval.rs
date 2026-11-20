//! Original payer-wallet approval and application of its pending proposal (Paykit wire 5.9).

use paykit_lib::allowances::Ledger;
use paykit_lib::bodies::ProposerRole;
use paykit_lib::records::{bind_ledger, AllowanceRecord, PendingAllowanceRecord};
use paykit_lib::{Body, Collection};
use pubky_switch_client::app::App;
use pubky_switch_core::ops::{Condition, RecordVersion};
use pubky_switch_core::shared::ApplicationRef;

use super::checkpoint::ledger_target;
use super::evidence::AppliedRecord;
use crate::allowances::{allowance_record_target, allowance_target, OriginalAllowanceApplication};
use crate::{Paykit, Result};

/// Reopened from this app's protected local acceptance before any grant or budget use.
pub(super) struct ApprovedOpening {
    pub(super) record: AllowanceRecord,
    pub(super) permanent: AppliedRecord,
    pub(super) ledger: AppliedRecord,
    pending_key: [u8; 32],
    pub(super) deletion: ApplicationRef,
}

impl ApprovedOpening {
    fn verify(app: &App, key: &[u8; 32], source: OriginalAllowanceApplication) -> Option<Self> {
        if !source.confirmed_for_execution {
            return None;
        }
        let id = paykit_lib::AllowanceId::parse(&source.allowance_id).ok()?;
        let target = allowance_target(&source.counterparty, &id);
        if &app.record_key(&target).ok()? != key {
            return None;
        }
        let permanent = AppliedRecord::retained(app, &target, &source.permanent)?;
        let own = app.identity();
        let record = AllowanceRecord::bind(&permanent.value, &own, app.naming_key(), key).ok()?;
        let Body::AllowanceProposal(p) = record.proposal.body() else {
            return None;
        };
        let reply = record.response.as_ref()?;
        let approval = match p.proposer_role {
            ProposerRole::Allower => &record.proposal,
            ProposerRole::Allowee => reply,
        };
        if record.counterparty != source.counterparty
            || record.allowance_id != id
            || source.decision_hash != approval.hash()
            || source.reply_hash != reply.hash()
            || source.generation != p.allower_generation
            || approval.sender() != own
            || approval.cert().app_id() != app.app_id()
            || permanent.cert.app_id() != app.app_id()
            || !permanent.cert.holds_scope("paykit_allowance_execute")
            || !record.grants.is_empty()
            || record.grant.is_some()
            || record.end.is_some()
            || record.conflicted.is_some()
        {
            return None;
        }
        let target = ledger_target(&source.counterparty, &id);
        let ledger = AppliedRecord::retained(app, &target, &source.ledger)?;
        bind_ledger(
            &ledger.value,
            app.naming_key(),
            &app.record_key(&target).ok()?,
            &record,
        )
        .ok()?;
        let q = reply.hash();
        if !permanent.created_together(&ledger)
            || ledger.value != Ledger::initial_value(&source.counterparty, &id, &q)
        {
            return None;
        }

        let pending =
            allowance_record_target(Collection::AllowanceProposals, &source.counterparty, &id);
        let selected = AppliedRecord::selected_proposal(app, &pending, &source.selected)?;
        let pending_key = app.record_key(&pending).ok()?;
        let placed =
            PendingAllowanceRecord::bind(&selected.value, &own, app.naming_key(), &pending_key)
                .ok()?;
        if placed.conflicted().is_some()
            || placed.proposal().hash() != record.proposal.hash()
            || placed.counterparty() != &source.counterparty
            || placed.allowance_id() != &id
        {
            return None;
        }
        let deletion = ledger.original_deletion(
            app,
            &pending,
            &selected,
            &source.selected_condition,
            &source.deletion,
        )?;
        Some(Self {
            record,
            permanent,
            ledger,
            pending_key,
            deletion,
        })
    }

    pub(super) fn grant_pins_pending(&self, grant: &AppliedRecord) -> bool {
        let key = self.pending_key;
        let retained = Condition::RecordCurrent {
            key,
            revision: self.deletion,
            deleted: true,
        };
        let forgotten = Condition::RecordVersion {
            key,
            version: RecordVersion::NONE,
        };
        let conditions = &grant.op.body().conditions;
        conditions.contains(&retained) || conditions.contains(&forgotten)
    }
}

impl Paykit {
    pub(super) async fn approved_opening(
        &mut self,
        record: &[u8; 32],
    ) -> Result<Option<ApprovedOpening>> {
        let source = self.original_allowance_application(record).await?;
        Ok(source.and_then(|s| ApprovedOpening::verify(&self.app, record, s)))
    }
}
