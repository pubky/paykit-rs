//! The original payer's individually signed withdrawal and the seller's matching return.
use super::*;
use crate::allowances::{end_names_acceptance, pending, replies, EndCause, EndOrigin};
use pubky_switch_core::records::IdentityDoc;
use pubky_switch_core::sig::SignedDoc;

pub(in crate::allowances) struct LiveOriginal {
    pub(super) condition: Condition,
    pub(super) member: KeptOp,
    pub(super) installed_version: RecordVersion,
}

/// Switch wire 7.4.14 assigns an upload older than the lost generation its lower
/// bound. A proved original create built on absence, so that bound is one.
pub(super) fn uploaded_create_version(original: RecordVersion, doc: &IdentityDoc) -> RecordVersion {
    if doc
        .body()
        .recovery_from
        .is_some_and(|lost| original.generation < lost)
    {
        RecordVersion::new(original.generation, 1)
    } else {
        original
    }
}

impl LiveOriginal {
    pub(in crate::allowances) fn bind_deletion(
        &self,
        write: &mut application::RecordWrite,
    ) -> Result<()> {
        let (conditions, OpKind::RecordPut(put)) = write else {
            return Err(PaykitError::corrupt());
        };
        if !conditions.contains(&self.condition) || !put.value_sealed.looks_like_delete() {
            return Err(unconfirmed());
        }
        put.expected_version = self.installed_version;
        Ok(())
    }
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
    returned: Option<LossReturn>,
}

/// The receive transaction takes custody before a fresh lifetime check can run.
#[derive(Clone, Serialize, Deserialize)]
enum LossReturn {
    Received(KeptMessage),
    Confirmed(KeptMessage),
}

impl LossReturn {
    fn message(&self) -> &KeptMessage {
        match self {
            Self::Received(e) | Self::Confirmed(e) => e,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvedDeletion {
    member: KeptOp,
    version: SeenVersion,
    /// The signed service-key history that dates a deletion applied after preparation.
    applied_under: Option<SignedDoc>,
}

fn carries_selection(prepared: &IdentityDoc, applied: &IdentityDoc, generation: u64) -> bool {
    prepared.generation() < generation
        && generation <= applied.generation()
        && prepared.check_successor(applied).is_ok()
        && applied.body().recovery_from == prepared.body().recovery_from
        && applied.service_key_of(prepared.generation()) == Some(prepared.inbox().service_key)
        && applied.service_key_of(generation).is_some()
        && crate::allowances::delivery::deletion_continues(
            prepared.generation(),
            applied.body().recovery_from,
        )
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
            let selected_in = doc.generation();
            let installed_version = uploaded_create_version(version, &doc);
            let generation = deletion.version.record_version().generation;
            let applied_doc = match &deletion.applied_under {
                Some(signed) => {
                    let authority =
                        IdentityDoc::verify(signed, &own).map_err(|_| PaykitError::corrupt())?;
                    if !carries_selection(&doc, &authority, generation) {
                        return Err(PaykitError::corrupt());
                    }
                    authority
                }
                None if generation == doc.generation() => doc,
                None => return Err(PaykitError::corrupt()),
            };
            let applied =
                application::applied_applications(vec![member.clone()], &applied_doc, app)?;
            let applied = applied
                .replies
                .get(&(w.cert().signing_key(), self.application))
                .ok_or_else(PaykitError::corrupt)?;
            let op = SignedOp::verify(&member.op, &w.cert().signing_key(), &own, Some(&app))
                .map_err(|_| PaykitError::corrupt())?;
            let selects_original = op.body().conditions.iter().any(|condition| {
                matches!(condition, Condition::RecordCurrent { key, revision, deleted: false }
                    if key == pending_key && revision.generation == selected_in
                        && revision.seq < applied.retirement.version && revision.item_hash == selected.item_hash)
            });
            let expected = matches!(&op.body().kind, OpKind::RecordPut(put) if put.expected_version == installed_version);
            if applied.retirement != deletion.version.record_version()
                || applied.pending_key != *pending_key
                || !expected
                || !selects_original
                || !op.body().conditions.contains(&pending::absent(*record))
            {
                return Err(PaykitError::corrupt());
            }
        }
        if let Some(returned) = &self.returned {
            let end = returned.message().stored(&own)?;
            if self.deletion.is_none()
                || (matches!(returned, LossReturn::Received(_))
                    && returned.message().position().is_none())
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
                    || matches!((&value.deletion,&imported.deletion),(Some(a),Some(b)) if a.member != b.member || a.version != b.version)
                {
                    return Err(PaykitError::corrupt());
                }
                value.deletion = value.deletion.or(imported.deletion);
                if value.returned.is_none()
                    || (matches!(value.returned, Some(LossReturn::Received(_)))
                        && matches!(imported.returned, Some(LossReturn::Confirmed(_))))
                {
                    value.returned = imported.returned;
                }
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

    pub(in crate::allowances) fn received_loss_return(&self) -> Option<&KeptMessage> {
        match self.settlement.as_ref()?.returned.as_ref()? {
            LossReturn::Received(end) => Some(end),
            LossReturn::Confirmed(_) => None,
        }
    }

    /// Reserve just the seller's matching return in the original settlement.
    /// Its local deletion may already be absent when the inbox accepts it.
    pub(in crate::allowances) fn keep_loss_return(
        &mut self,
        end: &StoredMessage,
        evidence: &KeptMessage,
        record: &[u8; 32],
        order: impl FnOnce(RecordVersion) -> Option<std::cmp::Ordering>,
    ) -> Result<Option<Vec<LocalWrite>>> {
        let Some(deletion) = self.return_retirement(end)? else {
            return Ok(None);
        };
        if matches!(
            order(deletion.record_version()),
            Some(std::cmp::Ordering::Equal | std::cmp::Ordering::Greater)
        ) {
            return Ok(None);
        }
        let s = self.settlement.as_mut().ok_or_else(PaykitError::corrupt)?;
        if let Some(previous) = &s.returned {
            return Ok(
                (previous.message().stored(&end.message().to)?.hash() == end.hash()).then(Vec::new),
            );
        }
        s.returned = Some(LossReturn::Received(evidence.clone()));
        Ok(Some(vec![state::local_put(
            &key::original_allowance(record),
            self,
        )?]))
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
            if previous.message().stored(&end.message().to)?.hash() != end.hash() {
                return Ok(None);
            }
            if matches!(previous, LossReturn::Confirmed(_)) {
                return Ok(Some(Vec::new()));
            }
        }
        s.returned = Some(LossReturn::Confirmed(KeptMessage::of(end)));
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
    pub(in crate::allowances) async fn resume_loss_return(
        &mut self,
        kept: &KeptMessage,
    ) -> Result<()> {
        let end = kept.stored(&self.identity())?;
        let id = named_allowance(end.body()).ok_or_else(PaykitError::corrupt)?;
        let record = self.app.record_key(&allowance_target(&end.sender(), id))?;
        self.finish_held_loss_return(&end, &record, kept, None)
            .await?;
        Ok(())
    }

    /// A held return can precede recovery of the local delete proof. Recheck its
    /// actual pending and permanent keys before releasing that retained message.
    pub(in crate::allowances) async fn finish_held_loss_return(
        &mut self,
        end: &StoredMessage,
        record: &[u8; 32],
        evidence: &KeptMessage,
        stop: Option<&str>,
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
        if let Some(stop) = stop {
            writes.push(LocalWrite::Delete {
                key: stop.as_bytes().to_vec(),
            });
        }
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
        if s.deletion.is_none() {
            let prepared = IdentityDoc::verify(&s.authority, &self.identity())
                .map_err(|_| PaykitError::corrupt())?;
            let generation = result.retirement.generation;
            let applied_under = if generation == prepared.generation() {
                None
            } else {
                let applied = self.app.own_identity();
                if !carries_selection(&prepared, applied, generation) {
                    return Ok(None);
                }
                Some(applied.signed().clone())
            };
            s.deletion = Some(ProvedDeletion {
                member: deletion.clone(),
                version: result.retirement.into(),
                applied_under,
            });
        }
        self.binding(|binder| original.check(record, binder))?;
        Ok(Some(state::local_put(&key, &original)?))
    }
}
