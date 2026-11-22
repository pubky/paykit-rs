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
use crate::state::{self, key, Tx};
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
    /// A repeated call requires the same app's current grant and the original or reconciled ledger.
    ///
    /// # Errors
    ///
    /// The original approval, its complete service application, or current authority is
    /// unavailable, the ledger cannot be reconciled, a later user decision is owed, or accessing
    /// the store or service failed.
    pub async fn resume_allowance_executor(&mut self, record: &[u8; 32]) -> Result<()> {
        Box::pin(self.apply_executor_grant(record, None)).await
    }

    /// Confirm this app's previously approved execution again after a consent hold.
    /// The host shows the counterparty, terms and executor. An earlier approval of
    /// automatic execution by this app is required (Paykit spec 8.3).
    /// This app can renew held consent against its own already incorporated ledger. A
    /// restored or unrecognized later ledger requires complete wallet history.
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
            let original_ledger = current_ledger.revision == opening.ledger.revision
                && accepted.application.revision == opening.ledger.revision;
            if fresh.revision != accepted.grant.revision
                || view.status != AllowanceStatus::Accepted
                || view.blocked != Some(false)
                || self
                    .local_get::<bool>(key::MISSED_CONSENTS)
                    .await?
                    .is_some()
            {
                return Err(held().into());
            }
            // Every check runs before anything is written, so a refused call leaves no
            // approval and no consent behind. The consent kept already stays as it is; with
            // none, the grant applied before its consent was written, and only the user's
            // approval under the current hold, or a confirmation now, consents.
            let kept = self
                .local_get::<ConsentEntry>(&key::consent(record))
                .await?;
            let consent = match kept.clone() {
                Some(entry) => entry,
                None if confirmed.is_some() || self.execution_approved(record).await? => {
                    ConsentEntry::new(
                        view.counterparty,
                        &view.allowance_id,
                        Consent::Held {
                            confirm_again: false,
                        },
                    )
                }
                None => return Err(held().into()),
            };
            if !matches!(consent.consent, Consent::Held { .. }) {
                return Err(held().into());
            }
            // A missed inbox entry can hold an already dispatched attempt. The user can
            // renew its consent against the exact ledger this installation incorporated.
            // Every new reservation still needs the method's complete history.
            let renews_known_consent = matches!(
                consent.consent,
                Consent::Held {
                    confirm_again: true
                }
            ) && confirmed.is_some()
                && !checkpoint.restored
                && current_ledger.revision == accepted.application.revision;
            if !original_ledger
                && !renews_known_consent
                && Box::pin(self.reconcile_allowance(record)).await?.is_none()
            {
                return Err(held().into());
            }
            let again = matches!(
                consent.consent,
                Consent::Held {
                    confirm_again: true
                }
            );
            if again && confirmed.is_none() {
                return Err(held().into());
            }
            // The approval and the consent are written together, in one local commit.
            let mut tx = Tx::default();
            if confirmed.is_some() {
                let hold = self.current_hold().await?;
                tx.put(&key::execution_approval(record, hold.as_ref()), &true)?;
            }
            if kept.is_none() || again {
                tx.put(
                    &key::consent(record),
                    &consent.with(Consent::Held {
                        confirm_again: false,
                    }),
                )?;
            }
            return self.local_commit(tx).await;
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
        // A first grant goes on the user's approval only under the hold it was given under:
        // after a restore that isn't clean or a `missed` item, every Allowance is held until
        // the user confirms it again (Paykit spec 7.2, 8.3). A confirmation now is that, and
        // is written only once every check below has passed.
        if confirmed.is_none() && !self.execution_approved(record).await? {
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
            restored: false,
            counterparty: view.counterparty,
            allowance_id: view.allowance_id.as_str().into(),
            later: None,
            signers: Default::default(),
            grant: GrantEvidence::AwaitingProof {
                value_hash: hash(&value),
            },
        };
        // The consent isn't in the batch: one kept and applied later could bring an Allowance
        // back over a removal, a newer decision or a newer hold. It is written only by
        // `promote_consent`, at once, once the grant applied.
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .entry(op)
            .local(state::local_put(&key, &ExecutorHistory(vec![checkpoint]))?);
        if confirmed.is_some() {
            let hold = self.current_hold().await?;
            self.local_put(&key::execution_approval(record, hold.as_ref()), &true)
                .await?;
        }
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
        self.promote_consent(record, &view.counterparty, &view.allowance_id)
            .await
    }

    /// Put the Allowance at `record` on the consent list once its grant applied, by a local
    /// commit made now, never by a batch the client may keep and apply later. It decides from
    /// the state now: a consent entry kept already, a removal or a newer decision, stays; while
    /// a `missed` item's mark waits nothing is consented; and only the user's approval under
    /// the current hold consents (Paykit spec 7.2, 8.3). Otherwise the Allowance waits for the
    /// user to confirm it ([`Paykit::confirm_allowance_executor`]).
    pub(super) async fn promote_consent(
        &mut self,
        record: &[u8; 32],
        counterparty: &[u8; 32],
        allowance_id: &paykit_lib::AllowanceId,
    ) -> Result<()> {
        if self
            .local_get::<ConsentEntry>(&key::consent(record))
            .await?
            .is_some()
            || self
                .local_get::<bool>(key::MISSED_CONSENTS)
                .await?
                .is_some()
            || !self.execution_approved(record).await?
        {
            return Ok(());
        }
        self.local_put(
            &key::consent(record),
            &ConsentEntry::new(
                *counterparty,
                allowance_id,
                Consent::Held {
                    confirm_again: false,
                },
            ),
        )
        .await
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
