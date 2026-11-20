//! The original payer's individually signed withdrawal and the seller's matching return.
use super::*;
use crate::allowances::{end_names_acceptance, pending, replies, EndCause, EndOrigin};
use pubky_switch_core::records::IdentityDoc;
use pubky_switch_core::sig::SignedDoc;

pub(in crate::allowances) struct LiveOriginal {
    pub(in crate::allowances) condition: Condition,
    pub(super) member: KeptOp,
}

/// The selected original, signed messages and service evidence stay together even
/// when neither Allowance collection has a live record (Paykit spec 8.3.1).
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LossSettlement {
    create: KeptOp,
    acceptance: KeptMessage,
    withdrawal: KeptMessage,
    #[serde(with = "serde_b64")]
    application: [u8; 32],
    authority: SignedDoc,
    deletion: Option<ProvedDeletion>,
    returned: Option<KeptMessage>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvedDeletion {
    member: KeptOp,
    version: SeenVersion,
}

impl LossSettlement {
    pub(super) fn check(
        &self,
        original: &OriginalAllowance,
        p: &StoredMessage,
        record: &[u8; 32],
        pending_key: &[u8; 32],
    ) -> Result<()> {
        let own = p.sender();
        let app = p.cert().app_id();
        let q = self.acceptance.stored(&own)?;
        let w = self.withdrawal.stored(&original.peer)?;
        let doc = IdentityDoc::verify(&self.authority, &own).map_err(|_| PaykitError::corrupt())?;
        let receipt = original
            .create_receipt
            .as_ref()
            .ok_or_else(PaykitError::corrupt)?;
        let (first, selected) = receipt.verify(&own, original.generation, pending_key)?;
        let made = application::applied_applications(vec![self.create.clone()], &first, app)?;
        let digest = original.create.ok_or_else(PaykitError::corrupt)?;
        let version = RecordVersion::new(selected.generation, selected.seq);
        if !original.confirmed_for_execution
            || !replies::answers(p, &q)
            || !matches!(q.body(), Body::AllowanceAcceptance(_))
            || q.accepted_at().is_none()
            || w.accepted_at().is_some()
            || w.cert().app_id() != app
            || original.reported_after_loss != Some(q.hash())
            || doc
                .body()
                .recovery_from
                .is_none_or(|lost| original.generation > lost)
            || made
                .pending_creates
                .get(&(*pending_key, digest))
                .is_none_or(|(at, proof)| *at != version || proof != &receipt.proof)
        {
            return Err(PaykitError::corrupt());
        }
        EndOrigin::of(*record, p, None, &w, EndCause::ProviderLoss)?;
        if let Some(deletion) = &self.deletion {
            let member = &deletion.member;
            let applied = application::applied_applications(vec![member.clone()], &doc, app)?;
            let applied = applied
                .replies
                .get(&(w.cert().signing_key(), self.application))
                .ok_or_else(PaykitError::corrupt)?;
            let op = SignedOp::verify(&member.op, &w.cert().signing_key(), &own, Some(&app))
                .map_err(|_| PaykitError::corrupt())?;
            let selects_original = op.body().conditions.iter().any(|condition| {
                matches!(condition, Condition::RecordCurrent { key, revision, deleted: false }
                    if key == pending_key && revision.generation == applied.retirement.generation
                        && revision.seq < applied.retirement.version && revision.item_hash == selected.item_hash)
            });
            let expected = matches!(&op.body().kind, OpKind::RecordPut(put) if put.expected_version == version);
            if applied.retirement != deletion.version.record_version()
                || applied.pending_key != *pending_key
                || applied.retirement.generation != doc.generation()
                || !expected
                || !selects_original
                || !op.body().conditions.contains(&pending::absent(*record))
            {
                return Err(PaykitError::corrupt());
            }
        }
        if let Some(returned) = &self.returned {
            let end = returned.stored(&own)?;
            if self.deletion.is_none()
                || end.accepted_at().is_none()
                || !end_names_acceptance(&end, &q)
            {
                return Err(PaykitError::corrupt());
            }
        }
        Ok(())
    }
}

impl OriginalAllowance {
    pub(super) fn merge_settlement(
        &mut self,
        installed: Option<Box<LossSettlement>>,
    ) -> Result<()> {
        if let Some(mut value) = installed {
            if let Some(imported) = self.settlement.take() {
                let own = self.decision.stored(&self.peer)?.sender();
                if value.create != imported.create
                    || value.application != imported.application
                    || value.authority != imported.authority
                    || value.acceptance.stored(&own)?.hash()
                        != imported.acceptance.stored(&own)?.hash()
                    || value.withdrawal.stored(&self.peer)?.hash()
                        != imported.withdrawal.stored(&self.peer)?.hash()
                    || matches!((&value.deletion,&imported.deletion),(Some(a),Some(b)) if a!=b)
                {
                    return Err(PaykitError::corrupt());
                }
                value.deletion = value.deletion.or(imported.deletion);
                value.returned = value.returned.or(imported.returned);
            }
            self.settlement = Some(value);
        }
        Ok(())
    }

    pub(in crate::allowances) fn owes_loss_withdrawal(&self, hash: [u8; 32]) -> Result<bool> {
        let Some(s) = &self.settlement else {
            return Ok(false);
        };
        Ok(s.deletion.is_some()
            && s.returned.is_none()
            && s.withdrawal.stored(&self.peer)?.hash() == hash)
    }

    /// A stop after saving D but before releasing W can outlive the service's
    /// copy of the tombstone. The typed local read verified the signed result.
    pub(in crate::allowances) fn saved_loss_application(
        &self,
        pending_key: [u8; 32],
        msg: &StoredMessage,
        commitment: [u8; 32],
    ) -> Result<Option<application::ConfirmedReply>> {
        let Some(s) = &self.settlement else {
            return Ok(None);
        };
        let Some(d) = &s.deletion else {
            return Ok(None);
        };
        if s.returned.is_some()
            || s.application != commitment
            || s.withdrawal.stored(&self.peer)?.hash() != msg.hash()
        {
            return Ok(None);
        }
        Ok(Some(application::ConfirmedReply {
            members: vec![d.member.clone()],
            pending_key,
            retirement: d.version.record_version(),
        }))
    }

    pub(in crate::allowances) fn settled_loss_deletion(&self) -> bool {
        self.settlement
            .as_ref()
            .is_some_and(|s| s.deletion.is_some())
    }

    fn return_retirement(&self, end: &StoredMessage) -> Result<Option<SeenVersion>> {
        let Some(s) = &self.settlement else {
            return Ok(None);
        };
        let Some(d) = &s.deletion else {
            return Ok(None);
        };
        Ok(
            end_names_acceptance(end, &s.acceptance.stored(&end.message().to)?)
                .then_some(d.version),
        )
    }

    /// Return the writes that retire this exact lifetime. The sender's retained
    /// message and the terminal host notice become durable together.
    pub(in crate::allowances) fn finish_loss_return(
        &mut self,
        end: &StoredMessage,
        record: &[u8; 32],
        retirement: RecordVersion,
    ) -> Result<Option<Vec<LocalWrite>>> {
        let Some(s) = self.settlement.as_mut() else {
            return Ok(None);
        };
        let Some(d) = &s.deletion else {
            return Ok(None);
        };
        if retirement != d.version.record_version()
            || !end_names_acceptance(end, &s.acceptance.stored(&end.message().to)?)
        {
            return Ok(None);
        }
        if let Some(previous) = &s.returned {
            return Ok((previous.stored(&end.message().to)?.hash() == end.hash()).then(Vec::new));
        }
        s.returned = Some(KeptMessage::of(end));
        let notice = crate::notices::KeptNotice::withdrawal_confirmed(end)?;
        Ok(Some(vec![
            state::local_put(&key::original_allowance(record), self)?,
            state::local_put(
                &key::message_notice(&end.sender(), &end.message().msg_id),
                &notice,
            )?,
        ]))
    }
}

impl Paykit {
    /// A held return can precede recovery of the local delete proof. Recheck its
    /// actual pending and permanent keys before releasing that retained message.
    pub(in crate::allowances) async fn finish_held_loss_return(
        &mut self,
        end: &StoredMessage,
        record: &[u8; 32],
        evidence: &KeptMessage,
        stop: &str,
    ) -> Result<bool> {
        let Some(mut original) = self
            .local_get::<OriginalAllowance>(&key::original_allowance(record))
            .await?
        else {
            return Ok(false);
        };
        let Some(retirement) = original.return_retirement(end)? else {
            return Ok(false);
        };
        let id = named_allowance(end.body()).ok_or_else(PaykitError::corrupt)?;
        let pending = allowance_record_target(Collection::AllowanceProposals, &end.sender(), id);
        let pending_key = self.app.record_key(&pending)?;
        let permanent = allowance_target(&end.sender(), id);
        let p = self.app.read_record_fresh(&pending).await?;
        let a = self.app.read_record_fresh(&permanent).await?;
        if crate::allowances::delivery::current(&pending, &pending_key, &p, true) != Some(None)
            || crate::allowances::delivery::current(&permanent, record, &a, false) != Some(None)
            || !evidence.position().is_some_and(|at| {
                replies::precedes_reply(retirement.record_version(), at, |a, b| {
                    self.app.received_order(a, b)
                })
            })
        {
            return Ok(false);
        }
        // A collected tombstone can be absent. The established retirement must
        // still account for that absence, including across an orderly change.
        let retired = self
            .local_get::<replies::PendingRetirement>(&key::pending_retirement(&pending_key))
            .await?;
        if !retired.is_some_and(|r| {
            r.withdrawal_after(retirement, self.app.own_identity().body().recovery_from)
                == pubky_switch_client::app::AppendDecision::Send
        }) {
            return Ok(false);
        }
        if matches!(&p,pubky_switch_client::app::RecordRead::Fresh(held) if held.version != retirement.record_version())
        {
            return Ok(false);
        }
        let Some(mut writes) =
            original.finish_loss_return(end, record, retirement.record_version())?
        else {
            return Ok(false);
        };
        writes.push(LocalWrite::Delete {
            key: stop.as_bytes().to_vec(),
        });
        self.app
            .protocol_commit(crate::protocol::BACKUP_NAME, &writes)
            .await?;
        Ok(true)
    }

    pub(in crate::allowances) async fn prepare_loss_settlement(
        &self,
        record: &[u8; 32],
        selected: LiveOriginal,
        acceptance: &StoredMessage,
        withdrawal: &StoredMessage,
        application: [u8; 32],
        tx: &mut Tx,
    ) -> Result<()> {
        let key = key::original_allowance(record);
        let mut original = self
            .local_get::<OriginalAllowance>(&key)
            .await?
            .ok_or_else(unconfirmed)?;
        if original.settlement.is_some() {
            return Err(unconfirmed());
        }
        original.settlement = Some(Box::new(LossSettlement {
            create: selected.member,
            acceptance: KeptMessage::of(acceptance),
            withdrawal: KeptMessage::of(withdrawal),
            application,
            authority: self.app.own_identity().signed().clone(),
            deletion: None,
            returned: None,
        }));
        self.binding(|binder| original.check(record, binder))?;
        tx.put(&key, &original)?;
        Ok(())
    }

    /// The matching W can enter delivery only with this protected local write and
    /// its applied-reply marker. A pre-result backup lacks authority to release it.
    pub(in crate::allowances) async fn capture_loss_deletion(
        &self,
        record: &[u8; 32],
        intent: &pending::PreparedPendingReply,
        result: &application::ConfirmedReply,
    ) -> Result<Option<LocalWrite>> {
        let key = key::original_allowance(record);
        let Some(mut original) = self.local_get::<OriginalAllowance>(&key).await? else {
            return Ok(None);
        };
        let Some(s) = original.settlement.as_mut() else {
            return Ok(None);
        };
        let [deletion] = result.members.as_slice() else {
            return Ok(None);
        };
        if s.application != intent.application
            || intent.fenced_reply(&self.identity())?
                != Some(s.acceptance.stored(&self.identity())?.hash())
            || s.withdrawal.stored(&intent.peer)?.hash()
                != intent.message.stored(&intent.peer)?.hash()
            || s.deletion
                .as_ref()
                .is_some_and(|known| &known.member != deletion)
            || s.returned.is_some()
        {
            return Ok(None);
        }
        s.deletion = Some(ProvedDeletion {
            member: deletion.clone(),
            version: result.retirement.into(),
        });
        self.binding(|binder| original.check(record, binder))?;
        Ok(Some(state::local_put(&key, &original)?))
    }
}
