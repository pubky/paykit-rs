//! Bind the native kept batch to its exact proposed ledger and preceding application.

use paykit_lib::allowances::Ledger;
use pubky_dispatch_client::app::{KeptBatchDrop, KeptBatchStatus, KeptOp, OpBatch};
use pubky_dispatch_core::enc::{b64, hash, serde_b64_vec};
use pubky_dispatch_core::ops::{Condition, OpKind, SignedOp};
use serde::{Deserialize, Serialize};

use super::checkpoint::{ledger_target, AcceptedLedger};
use super::evidence::AppliedRecord;
use super::spend::JournalAllowance;
use crate::state::{self, key, Binder};
use crate::{Paykit, PaykitError, Refusal, Result};

/// The wallet stages the complete proposal before handing the batch to Dispatch. Only
/// the ID Dispatch commits with its signed batch makes this a submitted preparation.
/// A crash before that commit leaves a proposal that was never sent.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerProposal {
    grant: JournalAllowance,
    before: KeptOp,
    #[serde(with = "serde_b64_vec")]
    result: Vec<u8>,
    standalone: bool,
}

impl LedgerProposal {
    pub(super) fn new(
        grant: &Condition,
        before: KeptOp,
        result: Vec<u8>,
        ledger: &Ledger,
        standalone: bool,
    ) -> Result<Self> {
        let Condition::RecordCurrent {
            revision,
            deleted: false,
            ..
        } = grant
        else {
            return Err(PaykitError::corrupt());
        };
        Ok(Self {
            grant: JournalAllowance::new(&ledger.allowance_id, *revision),
            before,
            result,
            standalone,
        })
    }

    pub(crate) fn bound(&self, binder: &Binder<'_>, record: &[u8; 32], digest: &[u8; 32]) -> bool {
        let verify = || -> Option<()> {
            if hash(&state::encode(self).ok()?) != *digest || !self.grant.valid() {
                return None;
            }
            let ledger = Ledger::from_json(&self.result).ok()?;
            if ledger.allowance_id != self.grant.id().ok()?
                || binder
                    .record_key(&crate::allowances::allowance_target(
                        &ledger.counterparty,
                        &ledger.allowance_id,
                    ))
                    .ok()?
                    != *record
            {
                return None;
            }
            let key = binder
                .record_key(&ledger_target(&ledger.counterparty, &ledger.allowance_id))
                .ok()?;
            let cert = paykit_lib::scopes::verify_certificate(&self.before.cert, binder.identity())
                .ok()?;
            let op = SignedOp::verify_with_cert(&self.before.op, &cert, binder.identity()).ok()?;
            let proof =
                pubky_dispatch_core::chain::TransitionProof::peek(self.before.proof.as_ref()?)
                    .ok()?;
            proof.check_binds(&op).ok()?;
            proof.record_revision().ok()?;
            matches!(&op.body().kind, OpKind::RecordPut(put) if put.key == key).then_some(())
        };
        verify().is_some()
    }

    pub(super) async fn stage(
        self,
        app: &mut Paykit,
        record: &[u8; 32],
        batch: OpBatch,
    ) -> Result<OpBatch> {
        let digest = hash(&state::encode(&self)?);
        let suffix = format!("{}/{}", b64(record), b64(&digest));
        // No operation can leave this wallet between these calls. The native ID is
        // written with the signed batch before transport, including a same-call reseal.
        app.local_put(&format!("{}{suffix}", key::LEDGER_PROPOSALS), &self)
            .await?;
        Ok(batch.keep_id_at(format!("{}{suffix}", key::LEDGER_BATCHES).into_bytes()))
    }
}

impl super::restore::ReconciledLedger {
    /// Stage the proposed plaintext and its verified base, then give Dispatch the key
    /// where it records the actual batch ID with the sealed and signed operations.
    pub(super) async fn stage(
        self,
        app: &mut Paykit,
        batch: OpBatch,
        extra: Vec<Condition>,
        standalone: bool,
    ) -> Result<OpBatch> {
        let Condition::RecordCurrent { key, .. } = self.grant else {
            return Err(PaykitError::corrupt());
        };
        let target = ledger_target(&self.ledger.counterparty, &self.ledger.allowance_id);
        let value = self.ledger.to_json()?;
        let mut conditions = vec![self.grant, self.current];
        conditions.extend(extra);
        let op = app.app.record_op(&target, Some(&value), conditions).await?;
        LedgerProposal::new(&self.grant, self.evidence, value, &self.ledger, standalone)?
            .stage(app, &key, batch.entry(op))
            .await
    }
}

#[derive(Default)]
pub(super) struct AppliedProposals {
    pub(super) suffixes: Vec<String>,
    // Only a proposal for the service's current write can describe how it was submitted.
    pub(super) current_standalone: Option<bool>,
}

impl Paykit {
    /// Only a native definitive refusal in uninterrupted history, or the original
    /// verified applied operation, resolves a submitted preparation (Paykit spec 8.3).
    pub(super) async fn prepared_ledger(
        &mut self,
        record: &[u8; 32],
        installed: &AcceptedLedger,
        current: &AppliedRecord,
        grant: &Condition,
    ) -> Result<Option<AppliedProposals>> {
        let prefix = format!("{}{}/", key::LEDGER_PROPOSALS, b64(record));
        let proposals = self.local_scan::<LedgerProposal>(&prefix).await?;
        let mut markers: std::collections::HashMap<_, _> = self
            .local_scan::<state::LedgerBatchIds>(&key::ledger_batches(record))
            .await?
            .into_iter()
            .collect();
        let mut applied = AppliedProposals::default();
        let mut applied_batches = std::collections::HashSet::new();
        let mut discarded = state::Tx::default();
        for (digest, proposal) in proposals {
            let suffix = format!("{}/{}", b64(record), digest);
            let marker = format!("{}{suffix}", key::LEDGER_BATCHES);
            let ids = markers.remove(&digest);
            let mut matched = false;
            if let Some(ids) = ids {
                let revision = proposal.grant.revision();
                let same_history = self
                    .app
                    .own_identity()
                    .body()
                    .recovery_from
                    .is_none_or(|lost| revision.generation > lost);
                for id in ids {
                    if !same_history {
                        return Ok(None);
                    }
                    // An imported marker can name the installed wallet's accepted
                    // application even after the service has advanced. The caller
                    // verifies that continuation before committing this cleanup.
                    if let Some(write) = [current, &installed.application]
                        .into_iter()
                        .find(|w| w.op.body().batch.id == id)
                    {
                        let ledger = Ledger::from_json(&proposal.result)?;
                        let target = ledger_target(&ledger.counterparty, &ledger.allowance_id);
                        if !applied_batches.insert(id)
                            || write.op.body().app_id != Some(self.app_id())
                            || !write.cert.holds_scope("paykit_allowance_execute")
                            || !write.op.body().conditions.contains(grant)
                            || !matches!(grant, Condition::RecordCurrent { revision: r, .. } if *r == revision)
                            // Written at the grant's generation or a later one of the
                            // same history: an orderly change or a cooperative move carries
                            // the grant it is conditioned on (Pubky Dispatch spec, Section 12.5).
                            || write.revision.generation < revision.generation
                            || write.value != proposal.result
                            // Staging retained the verified base. The signed write must still
                            // pin its exact revision after key retirement (Dispatch spec 4.3).
                            || AppliedRecord::established(&self.app, &target, &proposal.before)
                                .is_none_or(|base| !write.follows(&base))
                        {
                            return Ok(None);
                        }
                        if write.revision == current.revision {
                            applied.current_standalone = Some(proposal.standalone);
                        }
                        matched = true;
                    } else if !matches!(
                        self.app.kept_batch(id).await?,
                        Some(KeptBatchStatus::Dropped {
                            reason: KeptBatchDrop::Refused { .. } | KeptBatchDrop::Unsent,
                            ..
                        })
                    ) {
                        return Ok(None);
                    }
                }
            }
            if matched {
                applied.suffixes.push(suffix);
            } else {
                discarded.delete(&format!("{}{suffix}", key::LEDGER_PROPOSALS));
                discarded.delete(&marker);
            }
        }
        if !markers.is_empty() {
            return Err(PaykitError::corrupt());
        }
        let writes = discarded.into_writes();
        if !writes.is_empty() {
            self.app
                .protocol_commit(crate::protocol::BACKUP_NAME, &writes)
                .await?;
        }
        Ok(Some(applied))
    }
}

/// No executor checkpoint of this app for the Allowance, or one whose grant and ledger don't
/// verify: no read takes execution up, so it is shown (Paykit spec 8.3, "Executor").
pub(super) fn no_executor_base() -> PaykitError {
    Refusal::HeldForUser(crate::HeldForUserReason::AllowanceExecutionHeld(
        "this app can't establish its executor grant and ledger for the Allowance",
    ))
    .into()
}

/// The ledger write was applied, but the Dispatch's proof doesn't show the ledger at the
/// revision written: an answer that breaks a rule, never a refusal, which would say nothing
/// was written. The write stands; a later read shows the ledger.
pub(super) fn written_unverified() -> PaykitError {
    PaykitError::Lib(paykit_lib::Error::Invalid(
        "Paykit spec 8.3: the Allowance ledger write was applied, but its proof doesn't show \
         the ledger written"
            .into(),
    ))
}
