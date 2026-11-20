//! The proposer's original pending create and the rejection that retired it (Paykit spec 8.3.1).
use super::*;
use crate::allowances::{delivery, replies, AllowanceReply, AppliedReply, EndCause, EndOrigin};
use paykit_lib::kinds::Kind;
use pubky_switch_client::app::{AppendDecision, RecordApplication};
use pubky_switch_core::records::IdentityDoc;

/// Evidence observed at the service that applied the proposer's original write.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalCreate {
    authority: pubky_switch_core::sig::SignedDoc,
    pub(super) proof: pubky_switch_core::sig::SignedDoc,
}

impl OriginalCreate {
    pub(super) fn same_application(&self, other: &Self) -> bool {
        self.proof == other.proof
    }

    pub(super) fn verify(
        &self,
        own: &[u8; 32],
        generation: u64,
        key: &[u8; 32],
    ) -> Result<(IdentityDoc, ApplicationRef)> {
        let doc = IdentityDoc::verify(&self.authority, own).map_err(|_| PaykitError::corrupt())?;
        if doc.generation() != generation {
            return Err(PaykitError::corrupt());
        }
        let proof = TransitionProof::verify(&self.proof, &doc.inbox().service_key, own)
            .map_err(|_| PaykitError::corrupt())?;
        let revision = proof
            .record_revision()
            .map_err(|_| PaykitError::corrupt())?;
        if revision.generation != generation
            || proof.body().object != pubky_switch_core::enc::b64(key)
            || proof
                .body()
                .state
                .get("deleted")
                .and_then(|value| value.as_bool())
                != Some(false)
            || proof.envelope().batch.is_none_or(|b| b.count != 1)
        {
            return Err(PaykitError::corrupt());
        }
        Ok((doc, revision))
    }
}

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
    pub(in crate::allowances) fn awaits_loss_report(
        &self,
        reply: &StoredMessage,
        received_generation: u64,
    ) -> Result<bool> {
        if self.opening.is_some()
            || self.rejected.is_some()
            || self.reported_after_loss.is_some()
            || !self.confirmed_for_execution
            || self.create.is_none()
            || self.generation >= received_generation
            || !matches!(reply.body(), Body::AllowanceAcceptance(_))
        {
            return Ok(false);
        }
        Ok(replies::answers(&self.decision.stored(&self.peer)?, reply))
    }

    pub(in crate::allowances) fn awaiting_original_create(&self, current_generation: u64) -> bool {
        self.generation == current_generation
            && self.confirmed_for_execution
            && self.create.is_some()
            && self.create_receipt.is_none()
            && self.opening.is_none()
            && self.rejected.is_none()
    }

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
    /// Keep the service answer already bound by the native submitter, before delivery.
    pub(in crate::allowances) async fn keep_original_create(
        &mut self,
        record: &[u8; 32],
        proof: &TransitionProof,
    ) -> Result<()> {
        let key = key::original_allowance(record);
        let Some(mut original) = self.local_get::<OriginalAllowance>(&key).await? else {
            return Err(PaykitError::corrupt());
        };
        let doc = self.app.own_identity();
        if original.generation == doc.generation() && original.confirmed_for_execution {
            let receipt = OriginalCreate {
                authority: doc.signed().clone(),
                proof: proof.signed().clone(),
            };
            let decision = original.decision.stored(&original.peer)?;
            let id = named_allowance(decision.body()).ok_or_else(PaykitError::corrupt)?;
            let target =
                allowance_record_target(Collection::AllowanceProposals, &original.peer, id);
            receipt.verify(
                &self.identity(),
                original.generation,
                &self.app.record_key(&target)?,
            )?;
            original.create_receipt = Some(receipt);
            self.local_put(&key, &original).await?;
        }
        Ok(())
    }

    /// An unanswered proposal may settle during ordinary reopen or receive. Capture its
    /// signed result while the identity document still names the service that applied it.
    pub(in crate::allowances) async fn recover_original_create(
        &mut self,
        at: &str,
        original: &mut OriginalAllowance,
        confirmed: &application::AppliedApplications,
    ) -> Result<()> {
        let decision = original.decision.stored(&original.peer)?;
        let id = named_allowance(decision.body()).ok_or_else(PaykitError::corrupt)?;
        let key = self.app.record_key(&allowance_record_target(
            Collection::AllowanceProposals,
            &original.peer,
            id,
        ))?;
        let create = original.create.ok_or_else(PaykitError::corrupt)?;
        if let Some((version, proof)) = confirmed.pending_creates.get(&(key, create)) {
            if version.generation == original.generation {
                original.create_receipt = Some(OriginalCreate {
                    authority: self.app.own_identity().signed().clone(),
                    proof: proof.clone(),
                });
                self.local_put(at, original).await?;
            }
        }
        Ok(())
    }

    pub(in crate::allowances) fn fenced_own_proposal(&self, p: &StoredMessage) -> bool {
        matches!(p.body(), Body::AllowanceProposal(proposal)
            if proposal.proposer_role == ProposerRole::Allower
            && p.sender() == self.identity() && p.cert().app_id() == self.app_id()
            && self.app.own_identity().body().recovery_from
                .is_some_and(|lost| proposal.allower_generation <= lost))
    }

    /// Report the signed decision even if the current records do not authorize deletion.
    /// The same acceptance is reported once across acknowledgment, retries and reopen.
    pub(in crate::allowances) async fn report_fenced_acceptance(
        &mut self,
        record: &[u8; 32],
        target: &pubky_switch_client::app::RecordTarget,
        reply: &StoredMessage,
        evidence: &KeptMessage,
    ) -> Result<()> {
        if self.app.own_identity().body().recovery_from.is_none()
            || !matches!(reply.body(), Body::AllowanceAcceptance(_))
        {
            return Ok(());
        }
        let at = key::original_allowance(record);
        let mut tx = Tx::default();
        if let Some(mut original) = self.local_get::<OriginalAllowance>(&at).await? {
            if !original.awaits_loss_report(reply, self.app.own_identity().generation())?
                || !self.fenced_own_proposal(&original.decision.stored(&original.peer)?)
            {
                return Ok(());
            }
            original.reported_after_loss = Some(reply.hash());
            tx.put(&at, &original)?;
        } else {
            // The live signed P identifies the permanent refusal even if this backup
            // predates the app's create. Retain the reported disposition on every copy of Q.
            let pubky_switch_client::app::RecordRead::Fresh(live) =
                self.app.read_record_fresh(target).await?
            else {
                return Ok(());
            };
            let Some(pending) = replies::original_for(
                &live,
                target,
                &self.app.record_key(target)?,
                &self.identity(),
                reply,
            ) else {
                return Ok(());
            };
            if !self.fenced_own_proposal(pending.proposal()) {
                return Ok(());
            }
            let held_key = key::kept_reply(&reply.sender(), record, &reply.message().msg_id);
            let stored_key = key::stored_note(&reply.sender(), &reply.message().msg_id);
            let mut held = self.local_get::<KeptMessage>(&held_key).await?;
            let mut stored = self
                .local_get::<crate::stored::StoredNote>(&stored_key)
                .await?;
            let active = stored.as_mut().filter(|note| !note.placed);
            if held.is_none() && active.is_none() {
                return Ok(());
            }
            let mut reported = evidence.loss_reported;
            for q in held.iter_mut().chain(active.map(|note| &mut note.message)) {
                if q.stored(&self.identity())?.hash() != reply.hash() {
                    return Err(PaykitError::corrupt());
                }
                reported |= q.loss_reported;
                q.loss_reported = true;
            }
            if let Some(q) = held {
                tx.put(&held_key, &q)?;
            }
            if let Some(note) = stored.filter(|note| !note.placed) {
                tx.put(&stored_key, &note)?;
            }
            if reported {
                return self.local_commit(tx).await;
            }
        }
        let notice = crate::stored::given_up_notice(
            reply,
            self.service_time().unwrap_or(0),
            crate::notices::GivenUpOrigin::AllowanceBeforeLoss,
        );
        tx.put(
            &key::message_notice(&reply.sender(), &reply.message().msg_id),
            &notice,
        )?;
        self.local_commit(tx).await
    }

    /// Match the original service proof and the installed object, even when the
    /// replacement no longer carries the historical service key in its current identity.
    pub(in crate::allowances) async fn live_original_after_loss(
        &mut self,
        selected: &SharedRecord,
        proposal: &StoredMessage,
        reply: &StoredMessage,
    ) -> Result<Option<LiveOriginal>> {
        let Body::AllowanceProposal(p) = proposal.body() else {
            return Ok(None);
        };
        let peer = reply.sender();
        if !self.fenced_own_proposal(proposal)
            || !matches!(reply.body(), Body::AllowanceAcceptance(_))
            || !replies::answers(proposal, reply)
        {
            return Ok(None);
        }
        let permanent = self
            .app
            .record_key(&allowance_target(&peer, &p.allowance_id))?;
        let Some(approval) = self
            .local_get::<OriginalAllowance>(&key::original_allowance(&permanent))
            .await?
        else {
            return Ok(None);
        };
        let Some(create) = approval.create else {
            return Ok(None);
        };
        if approval.settled_loss_deletion()
            || approval.opening.is_some()
            || approval.rejected.is_some()
            || approval
                .reported_after_loss
                .is_some_and(|known| known != reply.hash())
            || approval.peer != peer
            || approval.generation != p.allower_generation
            || approval.decision.stored(&peer)?.hash() != proposal.hash()
            || self.unresolved_allowance(&selected.key).await?
            || self.unresolved_allowance(&permanent).await?
        {
            return Ok(None);
        }
        let target =
            allowance_record_target(Collection::AllowanceProposals, &peer, &p.allowance_id);
        let Some(RecordApplication {
            record: live,
            application: installed,
            evidence: mut member,
            installed_version,
            ..
        }) = self.app.read_record_application(&target).await?
        else {
            return Ok(None);
        };
        if live.version != selected.version
            || live.content != selected.content
            || live.writer != self.app_id()
            || installed.generation != self.app.own_identity().generation()
            || replies::original_for(&live, &target, &selected.key, &self.identity(), reply)
                .is_none()
        {
            return Ok(None);
        }
        let Some(receipt) = &approval.create_receipt else {
            return Ok(None);
        };
        let (original_doc, original) =
            receipt.verify(&self.identity(), approval.generation, &selected.key)?;
        if member
            .proof
            .as_ref()
            .is_some_and(|proof| proof != &receipt.proof)
            || original.item_hash != installed.item_hash
        {
            return Ok(None);
        }
        // The service can install exactly this signed operation without the old proof.
        // Verify it with the original payer's protected proof before using either version.
        member.proof.get_or_insert_with(|| receipt.proof.clone());
        let verified =
            application::applied_applications(vec![member.clone()], &original_doc, self.app_id())?;
        let condition = Condition::RecordCurrent {
            key: live.key,
            revision: installed,
            deleted: false,
        };
        let Some((version, _)) = verified.pending_creates.get(&(live.key, create)) else {
            return Ok(None);
        };
        Ok((settlement::valid_uploaded_create(
            installed_version,
            *version,
            self.app.own_identity(),
        ) && (live.version == *version || live.version == installed_version)
            && self.app.record_condition(&live.key).await? == condition)
            .then_some(LiveOriginal {
                condition,
                member,
                installed_version,
            }))
    }

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
            (Some((version, _)), _) if *version == selected.version => None,
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
                if verified
                    .pending_creates
                    .get(&(key, create))
                    .map(|(version, _)| version)
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
                || self.unresolved_allowance(&key).await?
                || self.unresolved_allowance(&record).await?
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
