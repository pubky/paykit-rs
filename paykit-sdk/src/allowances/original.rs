//! The approving payer app keeps the selected proposal and its exact original batch
//! before a grant can use either (Paykit wire 5.9). The state stays with that app.

use super::{
    allowance_record_target, allowance_target, application, named_allowance, Body, Paykit,
    StoredMessage,
};
use crate::divergent::SeenVersion;
use crate::error::{PaykitError, Refusal, Result};
use crate::records::KeptMessage;
use crate::state::{self, key, Binder, StateKey, Tx};
use paykit_lib::{bodies::ProposerRole, Collection};
use pubky_switch_client::app::{KeptOp, LocalWrite, RestoreContext, SharedRecord};
use pubky_switch_core::chain::TransitionProof;
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::ops::{Condition, OpKind, RecordVersion, SignedOp};
use pubky_switch_core::sealed::SealedValue;
use pubky_switch_core::shared::ApplicationRef;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OriginalApproval {
    #[serde(with = "serde_b64")]
    peer: [u8; 32],
    decision: KeptMessage,
    confirmed_for_execution: bool,
    generation: u64,
    opening: Option<Opening>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Opening {
    selected: KeptOp,
    version: SeenVersion,
    /// The exact installed application in its canonical Switch wire encoding when the
    /// deletion names record_current. A recovery upload can install an older signed write.
    current_revision: Option<Vec<u8>>,
    #[serde(with = "serde_b64")]
    reply: [u8; 32],
    #[serde(with = "serde_b64")]
    application: [u8; 32],
    seals: Vec<SealedValue>,
    applied: Option<[KeptOp; 3]>,
}

impl OriginalApproval {
    pub(super) fn new(
        peer: [u8; 32],
        signed: &StoredMessage,
        generation: u64,
        confirmed: bool,
    ) -> Self {
        Self {
            peer,
            decision: KeptMessage::of(signed),
            confirmed_for_execution: confirmed,
            generation,
            opening: None,
        }
    }

    pub(crate) fn check(&self, record: &[u8; 32], binder: &Binder<'_>) -> Result<()> {
        let decision = self.decision.stored(&self.peer)?;
        let id = named_allowance(decision.body()).ok_or_else(PaykitError::corrupt)?;
        if !self.confirmed_for_execution
            || self.generation == 0
            || decision.sender() != *binder.identity()
            || decision.accepted_at().is_some()
            || Some(decision.cert().app_id()) != binder.app_id()
            || binder.record_key(&allowance_target(&self.peer, id))? != *record
        {
            return Err(PaykitError::corrupt());
        }
        let matches_approval = match decision.body() {
            Body::AllowanceProposal(p) => {
                p.proposer_role == ProposerRole::Allower && p.allower_generation == self.generation
            }
            Body::AllowanceAcceptance(_) => self
                .opening
                .as_ref()
                .is_some_and(|o| o.reply == decision.hash()),
            _ => false,
        };
        if !matches_approval {
            return Err(PaykitError::corrupt());
        }
        if let Some(o) = &self.opening {
            if o.current_revision
                .as_deref()
                .is_some_and(|v| ApplicationRef::parse(v).is_err())
                || o.version.record_version().is_none()
                || o.selected.proof.is_none()
                || o.seals.len() != 3
                || o.seals[0].looks_like_delete()
                || o.seals[1].looks_like_delete()
                || !o.seals[2].looks_like_delete()
                || o.applied
                    .as_ref()
                    .is_some_and(|m| m.iter().any(|k| k.proof.is_none()))
            {
                return Err(PaykitError::corrupt());
            }
        }
        Ok(())
    }

    /// A matching batch can release the signed reply and preserve the exact payer
    /// receipt together, even when the original submission answer was lost.
    pub(super) fn applied(&mut self, commitment: [u8; 32], members: &[KeptOp]) -> Result<()> {
        let opening = self.opening.as_mut().ok_or_else(PaykitError::corrupt)?;
        let [a, l, d] = members else {
            return Err(PaykitError::corrupt());
        };
        if opening.application != commitment {
            return Err(PaykitError::corrupt());
        }
        let applied = [a.clone(), l.clone(), d.clone()];
        if opening
            .applied
            .as_ref()
            .is_some_and(|prior| prior != &applied)
        {
            return Err(PaykitError::corrupt());
        }
        opening.applied = Some(applied);
        Ok(())
    }

    fn merge(&mut self, newer: Self) -> Result<()> {
        if self.peer != newer.peer
            || self.generation != newer.generation
            || self.confirmed_for_execution != newer.confirmed_for_execution
            || self.decision.stored(&self.peer)?.hash()
                != newer.decision.stored(&newer.peer)?.hash()
        {
            return Err(PaykitError::corrupt());
        }
        match (&mut self.opening, newer.opening) {
            (Some(a), Some(mut b)) => {
                let applied = b.applied.take();
                let previous = a.applied.take();
                if a != &b || previous.is_some() && applied.is_some() && previous != applied {
                    return Err(PaykitError::corrupt());
                }
                a.applied = previous.or(applied);
            }
            (None, b) => self.opening = b,
            _ => (),
        }
        Ok(())
    }
}

pub(crate) fn restore_originals(
    restoring: &RestoreContext<'_>,
    binder: &Binder<'_>,
    mut kept: BTreeMap<[u8; 32], OriginalApproval>,
    writes: &mut Vec<LocalWrite>,
) -> Result<()> {
    for (at, v) in restoring.previous_local_scan(key::ORIGINAL_APPROVALS.as_bytes())? {
        let Some(StateKey::OriginalApproval(record)) = StateKey::parse(&at) else {
            return Err(PaykitError::corrupt());
        };
        let saved = state::restored::<OriginalApproval>(&at, &v, binder)?;
        if let Some(value) = kept.get_mut(&record) {
            value.merge(saved)?;
        } else {
            kept.insert(record, saved);
        }
    }
    for (record, value) in kept {
        writes.push(state::local_put(&key::original_approval(&record), &value)?);
    }
    Ok(())
}

fn unconfirmed() -> PaykitError {
    Refusal::NotNow("the approving wallet has no verified original Allowance application".into())
        .into()
}

impl Paykit {
    /// Store the original choice and selected signed proposal before submitting A/L/D.
    /// Retry reuses that exact application. A sibling, a different proposal or an older
    /// local choice cannot replace it.
    pub(super) async fn keep_original_selection(
        &mut self,
        selected: &SharedRecord,
        proposal: &StoredMessage,
        reply: &StoredMessage,
        operations: &mut [application::RecordWrite],
        confirmed: Option<bool>,
        tx: &mut Tx,
    ) -> Result<()> {
        if operations.len() != 3 {
            return Ok(());
        } // The seller records only its A and D.
        let Body::AllowanceProposal(p) = proposal.body() else {
            return Err(PaykitError::corrupt());
        };
        let (peer, decision) = match p.proposer_role {
            ProposerRole::Allower => (reply.sender(), proposal),
            ProposerRole::Allowee => (proposal.sender(), reply),
        };
        let id = &p.allowance_id;
        let record = self.app.record_key(&allowance_target(&peer, id))?;
        let at = key::original_approval(&record);
        if decision.sender() != self.identity()
            || decision.cert().app_id() != self.app_id()
            || !self.holds("paykit_allowance_execute")
        {
            return Err(unconfirmed());
        }
        let mut approval = match self.local_get::<OriginalApproval>(&at).await? {
            Some(held) => held,
            None if p.proposer_role == ProposerRole::Allowee => OriginalApproval::new(
                peer,
                decision,
                p.allower_generation,
                confirmed.ok_or_else(unconfirmed)?,
            ),
            None => return Err(unconfirmed()),
        };
        if approval.peer != peer
            || approval.generation != p.allower_generation
            || approval.decision.stored(&peer)?.hash() != decision.hash()
        {
            return Err(unconfirmed());
        }
        let pending = allowance_record_target(Collection::AllowanceProposals, &peer, id);
        let Some((current, revision, evidence)) =
            self.app.read_record_application(&pending).await?
        else {
            return Err(unconfirmed());
        };
        let (placed, _) = self.selected_pending(&current)?;
        if current.version != selected.version
            || current.content != selected.content
            || placed.conflicted().is_some()
            || placed.proposal().hash() != proposal.hash()
            || placed.counterparty() != &peer
            || placed.allowance_id() != id
        {
            return Err(unconfirmed());
        }
        let own = self.identity();
        let cert = paykit_lib::scopes::verify_certificate(&evidence.cert, &own)
            .map_err(|_| unconfirmed())?;
        let op = SignedOp::verify(
            &evidence.op,
            &cert.signing_key(),
            &own,
            Some(&cert.app_id()),
        )
        .map_err(|_| unconfirmed())?;
        let service = self
            .app
            .own_identity()
            .service_key_of(op.body().generation)
            .ok_or_else(unconfirmed)?;
        let proof = TransitionProof::verify(
            evidence.proof.as_ref().ok_or_else(unconfirmed)?,
            &service,
            &own,
        )
        .map_err(|_| unconfirmed())?;
        let original = proof.record_revision().map_err(|_| unconfirmed())?;
        let version = RecordVersion::new(original.generation, original.seq);
        if proof.check_binds(&op).is_err()
            || version != selected.version
            || !paykit_lib::scopes::certificate_in_force_at_write(
                self.app.own_identity(),
                &cert,
                proof.envelope().server_time,
            )
            || !matches!(&op.body().kind, OpKind::RecordPut(put) if put.key==current.key && put.policy==pending.collection.policy && !put.value_sealed.looks_like_delete())
        {
            return Err(unconfirmed());
        }
        let valid = Condition::RecordVersion {
            key: current.key,
            version,
        };
        let exact = Condition::RecordCurrent {
            key: current.key,
            revision,
            deleted: false,
        };
        let condition = self.app.record_condition(&current.key).await?;
        if (condition != exact && (condition != valid || original != revision))
            || !operations[2].0.contains(&condition)
        {
            return Err(unconfirmed());
        }
        let current_revision = (condition == exact)
            .then(|| revision.to_json())
            .transpose()
            .map_err(|_| unconfirmed())?;
        if let Some(previous) = &approval.opening {
            if previous.selected != evidence
                || previous.current_revision != current_revision
                || previous.reply != reply.hash()
                || previous.applied.is_some()
            {
                return Err(unconfirmed());
            }
            application::restore_seals(
                &self.app,
                operations,
                &previous.seals,
                previous.application,
            )?;
        } else {
            approval.opening = Some(Opening {
                selected: evidence,
                version: version.into(),
                current_revision,
                reply: reply.hash(),
                application: application::prepared_application(
                    own,
                    self.app_id(),
                    self.app.own_identity().generation(),
                    operations,
                )?,
                seals: application::seals(operations)?,
                applied: None,
            });
        }
        tx.put(&at, &approval)?;
        Ok(())
    }

    /// Keep the payer's original three signed writes and their verified service proofs.
    pub(super) async fn applied_original_approval(
        &mut self,
        record: &[u8; 32],
    ) -> Result<Option<OriginalApproval>> {
        let at = key::original_approval(record);
        let Some(mut approval) = self.local_get::<OriginalApproval>(&at).await? else {
            return Ok(None);
        };
        let Some(opening) = &mut approval.opening else {
            return Ok(None);
        };
        if opening.applied.is_none() {
            let confirmed = self.confirmed_pending_applications().await?;
            let Some(batch) = confirmed
                .replies
                .get(&(self.app.cert().signing_key(), opening.application))
            else {
                return Ok(None);
            };
            let commitment = opening.application;
            approval.applied(commitment, &batch.members)?;
            self.local_put(&at, &approval).await?;
        }
        Ok(Some(approval))
    }
}
