//! The proposer's original pending create and the rejection that retired it (Paykit spec 8.3.1).
use super::*;
use crate::allowances::{delivery, replies, AllowanceReply, AppliedReply, EndCause, EndOrigin};
use paykit_lib::kinds::Kind;
use pubky_switch_client::app::AppendDecision;
use pubky_switch_core::records::IdentityDoc;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Withdrawal {
    /// The rejection applied on this installation, before a restore or late acceptance.
    Unrequested,
    Owed,
    /// The end was queued, or verified evidence retired its lifetime.
    Finished,
    /// This backup cannot establish whether another installation already sent the end.
    Uncertain,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RejectedProposal {
    answer: KeptMessage,
    selected: SeenVersion,
    /// If the original service key was missing at rejection, retain its signed
    /// create and proof before the pending deletion replaces them.
    #[serde(deserialize_with = "Option::deserialize")]
    undated_create: Option<KeptOp>,
    /// An in-place import while the historical key is unavailable may restore
    /// the deletion without its proof, even on an uninterrupted service.
    #[serde(deserialize_with = "Option::deserialize")]
    deletion: Option<KeptOp>,
    #[serde(with = "serde_b64")]
    signer: [u8; 32],
    #[serde(with = "serde_b64")]
    application: [u8; 32],
    withdrawal: Withdrawal,
}

impl RejectedProposal {
    pub(super) fn check_answer(&self, proposal: &StoredMessage) -> Result<()> {
        let answer = self.answer.stored(&proposal.sender())?;
        if !matches!(answer.body(), Body::AllowanceRejection(_))
            || !replies::answers(proposal, &answer)
        {
            return Err(PaykitError::corrupt());
        }
        Ok(())
    }
}

fn proposal_history_is_known(doc: &IdentityDoc, generation: u64) -> bool {
    doc.service_key_of(generation).is_some()
        && delivery::deletion_continues(generation, doc.body().recovery_from)
}

impl OriginalAllowance {
    pub(in crate::allowances) fn with_pending_create(
        mut self,
        own: [u8; 32],
        app_id: [u8; 16],
        write: &application::RecordWrite,
    ) -> Result<Self> {
        self.create = Some(application::prepared_member(
            own,
            app_id,
            self.generation,
            1,
            &write.0,
            &write.1,
        )?);
        Ok(self)
    }

    /// An opening can coexist with an earlier rejection when a later pending write
    /// carried the same signed proposal. Its selected version belongs to that later write.
    pub(super) fn pending_lifetimes_are_ordered(&self) -> bool {
        self.rejected.as_ref().is_none_or(|r| {
            let selected = r.selected.record_version();
            selected.generation == self.generation
                && selected.version > 0
                && r.deletion
                    .iter()
                    .chain(&r.undated_create)
                    .all(|member| member.proof.is_some())
                && self.opening.as_ref().is_none_or(|o| r.selected < o.version)
        })
    }

    pub(super) fn merge_rejection(&mut self, installed: Option<RejectedProposal>) -> Result<()> {
        if let Some(mut installed) = installed {
            if let Some(other) = &self.rejected {
                let own = self.decision.stored(&self.peer)?.sender();
                if other.selected != installed.selected
                    || other.application != installed.application
                    || other.signer != installed.signer
                    || other.answer.stored(&own)?.hash() != installed.answer.stored(&own)?.hash()
                    || other.undated_create != installed.undated_create
                {
                    return Err(PaykitError::corrupt());
                }
                match (&installed.deletion, &other.deletion) {
                    (Some(a), Some(b)) if a != b => return Err(PaykitError::corrupt()),
                    (None, Some(deletion)) => installed.deletion = Some(deletion.clone()),
                    _ => {}
                }
                if other.withdrawal == Withdrawal::Finished {
                    installed.withdrawal = Withdrawal::Finished;
                }
            }
            self.rejected = Some(installed);
        }
        Ok(())
    }

    pub(super) fn request_installed_withdrawal(&mut self) {
        if let Some(rejected) = &mut self.rejected {
            if rejected.withdrawal == Withdrawal::Unrequested {
                rejected.withdrawal = Withdrawal::Owed;
            }
        }
    }

    pub(super) fn hold_imported_withdrawal(&mut self) {
        if let Some(rejected) = &mut self.rejected {
            if rejected.withdrawal != Withdrawal::Finished {
                rejected.withdrawal = Withdrawal::Uncertain;
            }
        }
    }

    pub(super) fn needs_rejection_proof(&self) -> bool {
        self.opening.is_none()
            && self.rejected.as_ref().is_some_and(|r| {
                r.deletion.is_none()
                    && matches!(r.withdrawal, Withdrawal::Unrequested | Withdrawal::Owed)
            })
    }

    pub(super) fn capture_rejection(
        &mut self,
        confirmed: &application::AppliedApplications,
    ) -> bool {
        let Some(rejected) = &mut self.rejected else {
            return false;
        };
        let Some(result) = confirmed
            .replies
            .get(&(rejected.signer, rejected.application))
        else {
            return false;
        };
        let [deletion] = result.members.as_slice() else {
            return false;
        };
        rejected.deletion = Some(deletion.clone());
        true
    }

    pub(in crate::allowances) fn withdrawal_owed(&self) -> bool {
        self.opening.is_none()
            && self
                .rejected
                .as_ref()
                .is_some_and(|r| r.withdrawal == Withdrawal::Owed)
    }
}

impl Paykit {
    /// These local writes take effect only when the native batch deletes the original
    /// signed create. A later installation of the same signed P has a different application.
    pub(in crate::allowances) async fn keep_proposal_rejection(
        &self,
        permanent: &[u8; 32],
        selected: &SharedRecord,
        proposal: &StoredMessage,
        reply: &StoredMessage,
        writes: &[application::RecordWrite],
    ) -> Result<Vec<LocalWrite>> {
        let at = key::original_allowance(permanent);
        let Some(mut approval) = self.local_get::<OriginalAllowance>(&at).await? else {
            return Ok(Vec::new());
        };
        let Some(create) = approval.create else {
            return Ok(Vec::new());
        };
        if approval.opening.is_some()
            || approval.rejected.is_some()
            || selected.version.generation != approval.generation
            || !delivery::deletion_continues(
                approval.generation,
                self.app.own_identity().body().recovery_from,
            )
            || approval.decision.stored(&approval.peer)?.hash() != proposal.hash()
        {
            return Ok(Vec::new());
        }
        let mut confirmed = self.confirmed_pending_applications().await?;
        let source = (selected.key, create);
        let undated_create = match (
            confirmed.pending_creates.get(&source),
            confirmed.undated_creates.remove(&source),
        ) {
            (Some(version), _) if *version == selected.version => None,
            (None, Some(member)) => Some(member),
            _ => return Ok(Vec::new()),
        };
        let rejected = RejectedProposal {
            answer: KeptMessage::of(reply),
            selected: selected.version.into(),
            undated_create,
            deletion: None,
            signer: self.app.cert().signing_key(),
            application: application::prepared_application(
                self.identity(),
                self.app_id(),
                approval.generation,
                writes,
            )?,
            withdrawal: Withdrawal::Unrequested,
        };
        rejected.check_answer(proposal)?;
        approval.rejected = Some(rejected);
        Ok(vec![state::local_put(&at, &approval)?])
    }

    pub(in crate::allowances) async fn request_late_withdrawal(
        &mut self,
        record: &[u8; 32],
        reply: &StoredMessage,
    ) -> Result<()> {
        let at = key::original_allowance(record);
        let Some(mut approval) = self.local_get::<OriginalAllowance>(&at).await? else {
            return Ok(());
        };
        if !approval
            .rejected
            .as_ref()
            .is_some_and(|r| r.withdrawal == Withdrawal::Unrequested)
            || !replies::answers(&approval.decision.stored(&approval.peer)?, reply)
        {
            return Ok(());
        }
        approval.request_installed_withdrawal();
        self.local_put(&at, &approval).await
    }

    pub(in crate::allowances) fn withdraw_rejected_proposal<'a>(
        &'a mut self,
        at: &'a str,
        mut approval: OriginalAllowance,
    ) -> pubky_switch_client::transport::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(rejected) = &mut approval.rejected else {
                return Ok(());
            };
            if rejected.withdrawal != Withdrawal::Owed
                || !self.allowance_end_send_permitted(&approval.peer).await?
            {
                return Ok(());
            }
            let proposal = approval.decision.stored(&approval.peer)?;
            let id = named_allowance(proposal.body()).ok_or_else(PaykitError::corrupt)?;
            let pending =
                allowance_record_target(Collection::AllowanceProposals, &approval.peer, id);
            let permanent = allowance_target(&approval.peer, id);
            let key = self.app.record_key(&pending)?;
            let record = self.app.record_key(&permanent)?;
            let Some(deletion) = &rejected.deletion else {
                return Ok(());
            };
            let pending_read = self.app.read_record_fresh(&pending).await?;
            let permanent_read = self.app.read_record_fresh(&permanent).await?;
            if !proposal_history_is_known(self.app.own_identity(), approval.generation) {
                return Ok(());
            }
            let members = std::iter::once(deletion)
                .chain(&rejected.undated_create)
                .cloned()
                .collect();
            let verified =
                application::applied_applications(members, self.app.own_identity(), self.app_id())?;
            let Some(result) = verified
                .replies
                .get(&(rejected.signer, rejected.application))
            else {
                return Ok(());
            };
            if result.members.len() != 1
                || result.pending_key != key
                || SeenVersion::from(result.retirement) <= rejected.selected
            {
                return Ok(());
            }
            if rejected.undated_create.is_some() {
                let create = approval.create.ok_or_else(PaykitError::corrupt)?;
                if verified.pending_creates.get(&(key, create))
                    != Some(&rejected.selected.record_version())
                {
                    return Ok(());
                }
            }
            let Some(retirement) = self
                .local_get::<replies::PendingRetirement>(&key::pending_retirement(&key))
                .await?
            else {
                return Ok(());
            };
            match retirement.withdrawal_after(
                result.retirement.into(),
                self.app.own_identity().body().recovery_from,
            ) {
                AppendDecision::Withdraw => {
                    rejected.withdrawal = Withdrawal::Finished;
                    return self.local_put(at, &approval).await;
                }
                AppendDecision::Hold => return Ok(()),
                AppendDecision::Send => (),
            }
            if delivery::current(&pending, &key, &pending_read, true) != Some(None)
                || delivery::current(&permanent, &record, &permanent_read, false) != Some(None)
                || self.conflict_kept(&record).await?
                || self.allowance_stopped(&record).await?
                || self
                    .local_get::<replies::PendingConflict>(&key::pending_conflict(&key))
                    .await?
                    .is_some()
            {
                return Ok(());
            }
            let body = AllowanceReply {
                allowance_id: id.clone(),
                executor_app_id: None,
            }
            .to_body(Kind::AllowanceEnd)?;
            let signed = self.sign(
                approval.peer,
                Kind::AllowanceEnd,
                vec![proposal.hash()],
                body,
            )?;
            let end = self.own_copy(&signed)?;
            let origin = EndOrigin::of(record, &proposal, None, &end, EndCause::RetiredProposal)?;
            rejected.withdrawal = Withdrawal::Finished;
            let local = [
                state::local_put(at, &approval)?,
                state::local_put(&key::end_origin(signed.msg_id()), &origin)?,
                state::local_put(
                    &key::applied_reply(&end.hash()),
                    &AppliedReply::new(&end, result.retirement),
                )?,
            ];
            let msg_id = self
                .app
                .queue_signed_with(crate::protocol::BACKUP_NAME, signed, &local)
                .await?;
            self.delivered(&msg_id).await?;
            Ok(())
        })
    }
}
