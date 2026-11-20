//! The first executor grant, conditioned on the original permanent ledger (Paykit spec 8.3).

use paykit_lib::grant::{grant_fields, EXECUTOR_GRANT_LABEL};
use paykit_lib::records::{AllowanceRecord, GrantEntry, Origin};
use paykit_lib::{Body, ExecutorGrant};
use pubky_switch_client::app::{KeptOp, OpBatch, RecordRead, RecordTarget, UserConfirmed};
use pubky_switch_core::enc::hash;
use pubky_switch_core::ops::Condition;

use super::approval::ApprovedOpening;
use super::checkpoint::{ledger_target, ExecutorCheckpoint, ExecutorHistory, GrantEvidence};
use super::evidence::AppliedRecord;
use crate::allowances::{allowance_target, Consent, ConsentEntry};
use crate::state::{self, key};
use crate::{AllowanceRole, AllowanceStatus, Paykit, Refusal, Result};

fn held() -> Refusal {
    Refusal::NotNow("the executor cannot establish the original Allowance grant and ledger".into())
}

impl Paykit {
    pub(super) async fn current_executor_record(
        &mut self,
        target: &RecordTarget,
    ) -> Result<(AppliedRecord, KeptOp)> {
        let RecordRead::Fresh(record) = self.app.read_record_fresh(target).await? else {
            return Err(held().into());
        };
        self.app
            .shared_evidence()
            .await?
            .ops
            .into_iter()
            .find_map(|op| Some((AppliedRecord::current(&self.app, target, &record, &op)?, op)))
            .ok_or_else(|| held().into())
    }

    /// Complete this app's previously authorized first grant after the original reply,
    /// including after reopening. An independent later hold requires its own confirmation.
    /// A repeated call requires the same app's current grant and unchanged original ledger.
    ///
    /// # Errors
    ///
    /// The original approval, its complete service application, or current authority is
    /// unavailable, the ledger has changed, a later user decision is owed, or accessing
    /// the store or service failed.
    pub async fn resume_allowance_executor(&mut self, record: &[u8; 32]) -> Result<()> {
        Box::pin(self.apply_executor_grant(record, None)).await
    }

    /// Confirm this app's previously approved execution again after a consent hold.
    /// The host shows the counterparty, terms and executor. An earlier approval of
    /// automatic execution by this app is required (Paykit spec 8.3).
    /// Repeating a grant requires this app's current first grant and the unchanged
    /// original ledger. This does not authorize continuation after spending.
    ///
    /// # Errors
    ///
    /// The original ledger, the user's standing consent, or the grant result cannot be
    /// established; another app changed either record; or the service or store failed.
    pub async fn confirm_allowance_executor(
        &mut self,
        record: &[u8; 32],
        confirmed: UserConfirmed,
    ) -> Result<()> {
        Box::pin(self.apply_executor_grant(record, Some(confirmed))).await
    }

    async fn apply_executor_grant(
        &mut self,
        record: &[u8; 32],
        confirmed: Option<UserConfirmed>,
    ) -> Result<()> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.records_known().await?;
        if !self.holds("paykit_allowance") {
            return Err(Refusal::ScopeMissing("paykit_allowance".into()).into());
        }
        if !self.holds("paykit_allowance_execute") {
            return Err(Refusal::ScopeMissing("paykit_allowance_execute".into()).into());
        }
        self.refuse_while_payer_pass_owed().await?;
        let opening = Box::pin(self.approved_opening(record))
            .await?
            .ok_or_else(held)?;
        let key = key::executor_checkpoint(record);
        if let Some(history) = self.local_get::<ExecutorHistory>(&key).await? {
            let checkpoint = history
                .selected(&self.app, record, &opening)
                .ok_or_else(held)?;
            let checkpoint = self
                .finish_executor_grant(record, checkpoint, &opening)
                .await?;
            let accepted = checkpoint
                .accepted(&self.app, record, &opening)
                .ok_or_else(held)?;
            let view = self.allowance(record).await?;
            let allow = allowance_target(&view.counterparty, &view.allowance_id);
            let (fresh, _) = self.current_executor_record(&allow).await?;
            let ledger = ledger_target(&view.counterparty, &view.allowance_id);
            let (current_ledger, _) = self.current_executor_record(&ledger).await?;
            if current_ledger.revision != opening.ledger.revision
                || fresh.revision != accepted.grant.revision
                || view.status != AllowanceStatus::Accepted
                || view.blocked != Some(false)
                || self
                    .local_get::<bool>(key::MISSED_CONSENTS)
                    .await?
                    .is_some()
            {
                return Err(held().into());
            }
            let consent = self
                .local_get::<ConsentEntry>(&key::consent(record))
                .await?
                .ok_or_else(held)?;
            if !matches!(consent.consent, Consent::Held { .. }) {
                return Err(held().into());
            }
            if let Consent::Held {
                confirm_again: true,
            } = consent.consent
            {
                if confirmed.is_none() {
                    return Err(held().into());
                }
                self.local_put(
                    &key::consent(record),
                    &consent.with(Consent::Held {
                        confirm_again: false,
                    }),
                )
                .await?;
            }
            return Ok(());
        }
        if self
            .local_get::<bool>(key::MISSED_CONSENTS)
            .await?
            .is_some()
            || self
                .local_get::<ConsentEntry>(&key::consent(record))
                .await?
                .is_some_and(|c| match c.consent {
                    Consent::Removed { .. } => true,
                    Consent::Held { confirm_again } => confirm_again && confirmed.is_none(),
                })
        {
            return Err(held().into());
        }
        let view = self.allowance(record).await?;
        if view.role != Some(AllowanceRole::Allower)
            || view.status != AllowanceStatus::Accepted
            || view.blocked != Some(false)
        {
            return Err(held().into());
        }
        let allow = allowance_target(&view.counterparty, &view.allowance_id);
        let led = ledger_target(&view.counterparty, &view.allowance_id);
        let (a, _) = self.current_executor_record(&allow).await?;
        let (l, _) = self.current_executor_record(&led).await?;
        let pending = Box::pin(self.pending_condition_for_executor(
            &view.counterparty,
            &view.allowance_id,
            opening.deletion,
        ))
        .await?
        .ok_or_else(held)?;
        if a.revision != opening.permanent.revision
            || l.revision != opening.ledger.revision
            || self.pause_at(record).await?.blocks_automatic()
            || self
                .pause_at(&self.app.record_key(&led)?)
                .await?
                .blocks_automatic()
        {
            return Err(held().into());
        }
        let own = self.identity();
        let mut next = AllowanceRecord::bind(&a.value, &own, self.app.naming_key(), record)?;
        let ledger_key = self.app.record_key(&led)?;
        let Body::AllowanceProposal(proposal) = next.proposal.body() else {
            return Err(held().into());
        };
        if !next.grants.is_empty()
            || next.grant.is_some()
            || next.end.is_some()
            || next.conflicted.is_some()
            || !super::in_grant_history(
                proposal.allower_generation,
                self.app.own_identity().generation(),
                self.app.own_identity().body().recovery_from,
            )
        {
            return Err(held().into());
        }
        let terms_hash = next.terms_hash().ok_or_else(held)?;
        let generation = self.app.own_identity().generation();
        let fields = grant_fields(
            &own,
            &view.counterparty,
            &view.allowance_id,
            &terms_hash,
            generation,
            a.version(),
        );
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        let grant = ExecutorGrant {
            app_id: self.app_id(),
            terms_hash,
            confirmed_generation: generation,
            cert: self.app.cert().signed().clone(),
            sig: self.app.sign_protocol(EXECUTOR_GRANT_LABEL, &refs)?,
            origin: Origin::default(),
        };
        next.grants.push(GrantEntry {
            built_on: a.version(),
            grant: grant.clone(),
            origin: Origin::default(),
        });
        next.grant = Some(grant);
        let value = next.to_json()?;
        let conditions = vec![
            Condition::RecordCurrent {
                key: *record,
                revision: a.revision,
                deleted: false,
            },
            Condition::RecordCurrent {
                key: ledger_key,
                revision: l.revision,
                deleted: false,
            },
            pending,
        ];
        let op = self.app.record_op(&allow, Some(&value), conditions).await?;
        let checkpoint = ExecutorCheckpoint {
            counterparty: view.counterparty,
            allowance_id: view.allowance_id.as_str().into(),
            grant: GrantEvidence::AwaitingProof {
                value_hash: hash(&value),
            },
        };
        let consent = ConsentEntry::new(
            view.counterparty,
            &view.allowance_id,
            Consent::Held {
                confirm_again: false,
            },
        );
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .entry(op)
            .local(state::local_put(&key, &ExecutorHistory(vec![checkpoint]))?)
            .local(state::local_put(&key::consent(record), &consent)?);
        self.app.submit_batch(batch).await?;
        let applied = self
            .local_get::<ExecutorHistory>(&key)
            .await?
            .ok_or_else(held)?;
        self.finish_executor_grant(
            record,
            applied
                .selected(&self.app, record, &opening)
                .ok_or_else(held)?,
            &opening,
        )
        .await?;
        Ok(())
    }

    pub(super) async fn finish_executor_grant(
        &mut self,
        record: &[u8; 32],
        mut checkpoint: ExecutorCheckpoint,
        opening: &ApprovedOpening,
    ) -> Result<ExecutorCheckpoint> {
        let GrantEvidence::AwaitingProof { value_hash } = &checkpoint.grant else {
            checkpoint
                .accepted(&self.app, record, opening)
                .ok_or_else(held)?;
            return Ok(checkpoint);
        };
        let id = paykit_lib::AllowanceId::parse(&checkpoint.allowance_id)?;
        let target = allowance_target(&checkpoint.counterparty, &id);
        let previous = &opening.permanent;
        let (grant, write) = self.current_executor_record(&target).await?;
        if !grant.follows(previous) || &hash(&grant.value) != value_hash {
            return Err(held().into());
        }
        checkpoint.grant = GrantEvidence::Verified { write };
        checkpoint
            .accepted(&self.app, record, opening)
            .ok_or_else(held)?;
        self.local_put(
            &key::executor_checkpoint(record),
            &ExecutorHistory(vec![checkpoint.clone()]),
        )
        .await?;
        Ok(checkpoint)
    }
}
