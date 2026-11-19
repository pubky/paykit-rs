//! Admission, views and replies to current pending Allowance proposals.

use super::{
    allowance_record_target, allowance_target, AllowanceStatus, AllowanceView, Paykit,
    PendingConflict,
};
use crate::allowances::{AllowanceReply, AppliedReply, EndCause, EndOrigin};
use crate::error::{PaykitError, Refusal, Result};
use crate::records::KeptMessage;
use crate::state::{self, key};
use crate::stored::{submitted, Placed, StoredNote};
use paykit_lib::collections::Collection;
use paykit_lib::kinds::Kind;
use paykit_lib::records::PendingAllowanceRecord;
use pubky_switch_client::app::{
    AppError, KeptReason, LocalWrite, OpBatch, RecordContent, RecordRead, RecordTarget,
    SharedRecord,
};
use pubky_switch_core::chain::bind_answer;
use pubky_switch_core::enc::{hash, serde_b64};
use pubky_switch_core::ops::{BatchRef, Condition, OpBody, OpKind, RecordVersion, SignedOp};
use pubky_switch_core::records::AppCert;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// An original, signed answer the user already chose for this pending record. Kept before
/// submission, so a restart while settlement is unknown cannot prepare a second answer.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedPendingReply {
    #[serde(with = "serde_b64")]
    pub(crate) peer: [u8; 32],
    pub(crate) message: KeptMessage,
    /// Commitment to this reply's exact sealed delete and original application conditions.
    #[serde(with = "serde_b64")]
    pub(crate) delete: [u8; 32],
}

/// The Switch chooses the batch ID during submission. All other signed members, including
/// the random seal and conditions, identify the exact record write this app prepared.
fn prepared_write(
    identity: [u8; 32],
    app_id: [u8; 16],
    generation: u64,
    conditions: &[Condition],
    kind: &OpKind,
) -> Result<[u8; 32]> {
    let body = OpBody {
        identity,
        app_id: Some(app_id),
        generation,
        batch: BatchRef {
            id: [0; 16],
            count: 1,
        },
        conditions: conditions.to_vec(),
        kind: kind.clone(),
    };
    Ok(hash(&body.to_json().map_err(|_| PaykitError::corrupt())?))
}

struct ConfirmedWrite {
    certificate: [u8; 32],
    version: RecordVersion,
}

fn not_current() -> PaykitError {
    Refusal::NotNow("the current pending proposal and Allowance could not be verified".into())
        .into()
}

pub(super) fn absent(key: [u8; 32]) -> Condition {
    Condition::RecordVersion {
        key,
        version: RecordVersion::NONE,
    }
}

pub(super) fn is_create(op: &OpKind) -> bool {
    matches!(op, OpKind::RecordPut(put) if put.expected_version == RecordVersion::NONE && put.id_inputs.is_some())
}

pub(super) const LIMIT: usize = 8;

impl Paykit {
    pub(crate) async fn pending_count(
        &mut self,
        report: Option<&pubky_switch_client::app::ReceiveReport>,
        peer: &[u8; 32],
        outgoing: bool,
    ) -> Result<Option<usize>> {
        let Some(report) = report else {
            return Ok(None);
        };
        let family = crate::activity::family(Collection::AllowanceProposals);
        let Some(current) = self.app.current_collection(&family, report).await? else {
            return Ok(None);
        };
        if !current.uncertain_keys.is_empty() {
            return Ok(None);
        }
        let mut count = 0;
        for held in current.live {
            let proposal = match self.selected_pending(&held) {
                Ok((proposal, _)) => proposal,
                Err(error @ PaykitError::Client(_)) => return Err(error),
                Err(_) => return Ok(None),
            };
            if proposal.counterparty() == peer
                && (proposal.proposal().sender() == self.identity()) == outgoing
            {
                count += 1;
            }
        }
        Ok(Some(count))
    }

    pub(super) async fn absent_record(&mut self, target: &RecordTarget) -> Result<()> {
        match self.app.read_record_fresh(target).await? {
            RecordRead::Kept {
                reason: KeptReason::NotFound,
                record: None,
            } => Ok(()),
            _ => Err(not_current()),
        }
    }

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
    pub(super) async fn pending_by_allowance_key(
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

    /// Project a verified pending proposal at its canonical Allowance key. A retained
    /// conflict still shows the original terms until its pending lifetime ends. Only the
    /// resend inventory can use a partial list of already identified counterparties.
    pub(super) async fn pending_views(
        &self,
        wanted: Option<&[u8; 32]>,
        allow_partial: bool,
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
            by_key.insert(self.app.record_key(&target)?, (pending, true));
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
                Ok((pending, _)) => {
                    let conflicted = pending.conflicted().is_some();
                    by_key.insert(held.key, (pending, conflicted));
                }
                Err(_) => unresolved = true,
            }
        }
        let mut out = Vec::new();
        for (pending, conflicted) in by_key.into_values() {
            let permanent = self.app.record_key(&allowance_target(
                pending.counterparty(),
                pending.allowance_id(),
            ))?;
            if wanted.is_some_and(|key| *key != permanent)
                || self.app.shared_record(&permanent).await?.is_some()
            {
                continue;
            }
            let status = if conflicted || self.conflict_kept(&permanent).await? {
                AllowanceStatus::Conflicted
            } else if self.allowance_stopped(&permanent).await? {
                AllowanceStatus::Ended
            } else {
                AllowanceStatus::Proposed
            };
            out.push(Box::pin(self.proposal_view(permanent, pending.proposal(), status)).await?);
        }
        if !allow_partial && unresolved && (wanted.is_none() || out.is_empty()) {
            return Err(not_current());
        }
        Ok(out)
    }

    /// The current service's complete results for this installation's original pending writes.
    async fn confirmed_pending_writes(
        &self,
        deleted: bool,
    ) -> Result<BTreeMap<([u8; 32], [u8; 32]), ConfirmedWrite>> {
        let own = self.identity();
        let app_id = self.app_id();
        let doc = self.app.own_identity();
        let generation = doc.generation();
        let Some(service) = doc.service_key_of(generation) else {
            return Ok(BTreeMap::new());
        };
        let mut confirmed = BTreeMap::new();
        for held in self.app.shared_evidence().await?.ops {
            let Some(proof) = held.proof else { continue };
            let Ok(cert) = AppCert::verify(&held.cert, &own) else {
                continue;
            };
            if cert.app_id() != app_id {
                continue;
            }
            let Ok(op) = SignedOp::verify(&held.op, &cert.signing_key(), &own, Some(&app_id))
            else {
                continue;
            };
            let body = op.body();
            let OpKind::RecordPut(put) = &body.kind else {
                continue;
            };
            if body.generation != generation
                || body.batch.count != 1
                || put.policy != Collection::AllowanceProposals.fixed_policy()
                || put.value_sealed.looks_like_delete() != deleted
            {
                continue;
            }
            let Ok(proofs) = bind_answer(std::slice::from_ref(&op), &[proof], &service, &own)
            else {
                continue;
            };
            let [proof] = proofs.as_slice() else { continue };
            if (cert.issued_at()..cert.expires_at()).contains(&proof.envelope().server_time) {
                if let Some(version) = proof.body().version {
                    let operation =
                        prepared_write(own, app_id, generation, &body.conditions, &body.kind)?;
                    confirmed.insert(
                        (put.key, operation),
                        ConfirmedWrite {
                            certificate: cert.hash(),
                            version: RecordVersion::new(generation, version),
                        },
                    );
                }
            }
        }
        Ok(confirmed)
    }

    /// Restore a prepared reply only from the signed operation and the complete service
    /// result for its exact delete. A retained tombstone or a hosted absence alone cannot
    /// tell whether this app's write applied.
    pub(crate) async fn recover_pending_deletes(&mut self) -> Result<()> {
        let intents = self
            .local_scan::<PreparedPendingReply>(key::PENDING_REPLIES)
            .await?;
        if intents.is_empty() {
            return Ok(());
        }
        let deletes = self.confirmed_pending_writes(true).await?;
        for (at, intent) in intents {
            let msg = intent.message.stored(&intent.peer)?;
            let id = super::named_allowance(msg.body()).ok_or_else(PaykitError::corrupt)?;
            let target = allowance_record_target(Collection::AllowanceProposals, &intent.peer, id);
            let pending_key = self.app.record_key(&target)?;
            if deletes
                .get(&(pending_key, intent.delete))
                .is_none_or(|write| write.certificate != msg.cert().hash())
            {
                continue;
            }
            // Queue the signed original first: its release mark is absent until the commit
            // below. A stop between the two leaves both the intent and original recoverable.
            let queue = self.app.queue_stored(msg.plaintext()).await;
            let mut writes = vec![LocalWrite::Delete {
                key: format!("{}{at}", key::PENDING_REPLIES).into_bytes(),
            }];
            let msg_id = match queue {
                Ok(msg_id) => msg_id,
                Err(pubky_switch_client::app::AppError::Refused(
                    pubky_switch_client::app::Refusal::WontDeliver(_),
                )) => {
                    let msg_id = &msg.message().msg_id;
                    let notice_key = key::message_notice(&intent.peer, msg_id);
                    if self.kept_notice(&notice_key).await?.is_none() {
                        let notice = crate::notices::KeptNotice::given_up(
                            intent.peer,
                            msg_id,
                            None,
                            self.service_time().unwrap_or(0),
                            crate::notices::GivenUpFrom::Origin(
                                crate::notices::GivenUpOrigin::HeldAnswer,
                            ),
                        );
                        writes.push(state::local_put(&notice_key, &notice)?);
                    }
                    self.app
                        .protocol_commit(crate::protocol::BACKUP_NAME, &writes)
                        .await?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            writes.push(state::local_put(
                &key::applied_reply(&msg.hash()),
                &AppliedReply {
                    created_at_ms: msg.message().created_at_ms,
                    reply: msg.hash(),
                    proposal: msg.message().refs[0],
                },
            )?);
            self.app
                .protocol_commit(crate::protocol::BACKUP_NAME, &writes)
                .await?;
            self.delivered(&msg_id).await?;
        }
        Ok(())
    }

    /// Resume the previously signed reply before interpreting a missing or changed record.
    /// The submitter's batch or outbox retains the original message while its answer is owed.
    pub(super) async fn resume_pending_answer(
        &mut self,
        key: &[u8; 32],
        kind: Kind,
    ) -> Result<Option<String>> {
        let Some(intent) = self
            .local_get::<PreparedPendingReply>(&key::pending_reply(key))
            .await?
        else {
            return Ok(None);
        };
        let previous = intent.message.stored(&intent.peer)?;
        if previous.body().kind() != kind {
            return Err(Refusal::NotNow(
                "an earlier answer to this proposal is awaiting settlement".into(),
            )
            .into());
        }
        let msg_id = &previous.message().msg_id;
        match self.send_message(msg_id).await? {
            Some(
                pubky_switch_client::app::Delivery::Kept { .. }
                | pubky_switch_client::app::Delivery::Queued { .. }
                | pubky_switch_client::app::Delivery::Accepted { .. },
            ) => Ok(Some(msg_id.clone())),
            _ => Err(Refusal::NotNow(
                "the original Allowance answer has no verified outcome".into(),
            )
            .into()),
        }
    }

    /// Open only the current pending value at the user's selected key, followed by its
    /// permanent record. The cached value supplies the natural key for the first read.
    pub(super) async fn answer_pending(
        &mut self,
        selected: &SharedRecord,
        kind: Kind,
    ) -> Result<String> {
        let own = self.identity();
        if kind == Kind::AllowanceAcceptance {
            return Err(Refusal::AllowanceAcceptanceUnavailable.into());
        }
        let (chosen, target) = self.selected_pending(selected)?;
        let id = chosen.allowance_id();
        let peer = chosen.counterparty();
        let condition = self.app.record_condition(&selected.key).await?;
        let RecordRead::Fresh(held) = self.app.read_record_fresh(&target).await? else {
            return Err(not_current());
        };
        let (pending, _) = self.selected_pending(&held)?;
        if held.content != selected.content
            || self.app.record_condition(&held.key).await? != condition
        {
            return Err(not_current());
        }
        let permanent = allowance_target(peer, id);
        self.absent_record(&permanent).await?;
        let permanent_key = self.app.record_key(&permanent)?;
        self.refresh_own()?;
        self.require_allowance_send(kind)?;
        if pending.conflicted().is_some()
            || self.conflict_kept(&permanent_key).await?
            || self
                .local_get::<super::PendingConflict>(&key::pending_conflict(&selected.key))
                .await?
                .is_some()
        {
            return Err(Refusal::AllowanceConflicted.into());
        }
        if self.allowance_stopped(&permanent_key).await? {
            return Err(Refusal::AllowanceEnded.into());
        }
        let p = pending.proposal();
        if (kind == Kind::AllowanceEnd) != (p.sender() == own) {
            return Err(Refusal::Usage(
                "only the proposer withdraws a pending Allowance and only its recipient answers it"
                    .into(),
            )
            .into());
        }
        if kind != Kind::AllowanceEnd {
            self.refuse_while_sends_held(peer).await?;
        }
        let intent_key = key::pending_reply(&permanent_key);
        let body = AllowanceReply {
            allowance_id: id.clone(),
            executor_app_id: None,
        }
        .to_body(kind)?;
        let reply = self.sign(*peer, kind, vec![p.hash()], body)?;
        let q = self.own_copy(&reply)?;
        let mut batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME);
        // Guard the very version the user answered. A recovery upload also gets the
        // client's exact application condition; a concurrent change refuses the batch.
        let delete = self
            .app
            .record_op(&target, None, vec![condition, absent(permanent_key)])
            .await?;
        let delete_hash = prepared_write(
            own,
            self.app_id(),
            self.app.own_identity().generation(),
            &delete.0,
            &delete.1,
        )?;
        batch = batch.entry(delete);
        batch = batch.local(state::local_put(
            &key::applied_reply(&q.hash()),
            &AppliedReply {
                created_at_ms: q.message().created_at_ms,
                reply: q.hash(),
                proposal: p.hash(),
            },
        )?);
        let msg_id = reply.msg_id().to_owned();
        let mut prepare = state::Tx::default();
        prepare.put(
            &intent_key,
            &PreparedPendingReply {
                peer: *peer,
                message: KeptMessage::of(&q),
                delete: delete_hash,
            },
        )?;
        if kind == Kind::AllowanceEnd {
            prepare.put(
                &key::end_origin(&msg_id),
                &EndOrigin::of(permanent_key, p, None, &q, EndCause::User)?,
            )?;
        }
        self.local_commit(prepare).await?;
        batch = batch.local(LocalWrite::Delete {
            key: intent_key.as_bytes().to_vec(),
        });
        match self.submit(batch.queue(reply)).await {
            Ok(_) => {
                self.delivered(&msg_id).await?;
            }
            Err(error) if error.kept_batch().is_some() => (),
            Err(error) => {
                // An earlier batch can fail before this one is signed. These local
                // refusals likewise mean it was never kept or sent. An unreadable
                // store leaves its outcome unknown, so the original stays prepared.
                if matches!(error, AppError::Ahead { .. } | AppError::Refused(_))
                    || error.code() == Some(pubky_switch_core::http::ErrorCode::Conflict)
                {
                    self.local_delete(&intent_key).await?;
                }
                return Err(error.into());
            }
        }
        Ok(msg_id)
    }

    /// Admit the signed original only after the read establishes this counterparty's
    /// incoming count. A record retained by another wallet keeps its ordinary
    /// competing-proposal rule (Paykit spec 8.3.1).
    pub(super) async fn place_incoming_proposal(
        &mut self,
        message: &super::StoredMessage,
        evidence: &KeptMessage,
        stored_id: Option<u64>,
        report: Option<&pubky_switch_client::app::ReceiveReport>,
    ) -> Result<Option<Placed>> {
        let own = self.identity();
        let peer = message.sender();
        let id = super::named_allowance(message.body()).ok_or_else(PaykitError::corrupt)?;
        let pending = allowance_record_target(Collection::AllowanceProposals, &peer, id);
        let permanent = allowance_target(&peer, id);
        let p_key = self.app.record_key(&pending)?;
        let a_key = self.app.record_key(&permanent)?;
        let p_read = self.app.read_record_fresh(&pending).await?;
        let a_read = self.app.read_record_fresh(&permanent).await?;
        let (Some(p), Some(a)) = (
            super::delivery::current(&pending, &p_key, &p_read, true),
            super::delivery::current(&permanent, &a_key, &a_read, false),
        ) else {
            return Ok(Some(Placed::Waits));
        };
        if a.is_some() {
            return Ok(p.is_some().then_some(Placed::Waits));
        }
        let original = message.message().msg_id.as_str();
        let mut effects = state::Tx::default();
        let held = if let Some(stored_id) = stored_id {
            let key = key::stored_note(&peer, original);
            let Some(mut note) = self.local_get::<StoredNote>(&key).await? else {
                return Ok(Some(Placed::Done));
            };
            if note.placed || note.stored_id != stored_id {
                return Ok(Some(Placed::Done));
            }
            note.placed = true;
            effects.put(&key, &note)?;
            note.message
        } else {
            let key = key::kept_reply(&peer, &a_key, original);
            let Some(held) = self.local_get::<KeptMessage>(&key).await? else {
                return Ok(Some(Placed::Done));
            };
            effects.delete(&key);
            held
        };
        let position = evidence.position().ok_or_else(PaykitError::corrupt)?;
        if held.position() != Some(position) || held.stored(&own)?.hash() != message.hash() {
            return Ok(Some(Placed::Waits));
        }
        if p.is_some() {
            return self.place_received_second(message, &held, stored_id).await;
        }
        if let Some(end) = self
            .local_get::<super::PendingRetirement>(&key::pending_retirement(&p_key))
            .await?
        {
            match end.compare(None, Some(position), |a, b| self.app.received_order(a, b)) {
                Some(std::cmp::Ordering::Greater) => (),
                Some(std::cmp::Ordering::Less) => return Ok(Some(Placed::Done)),
                _ => return Ok(Some(Placed::Waits)),
            }
        }
        let retained_delete = match &p_read {
            RecordRead::Fresh(record)
            | RecordRead::Kept {
                record: Some(record),
                ..
            } => Some(record),
            _ => None,
        };
        if let Some(deleted) = retained_delete {
            let delete_at = pubky_switch_client::app::FeedPosition {
                generation: deleted.version.generation,
                seq: deleted.version.version,
            };
            match self.app.received_order(position, delete_at) {
                Some(std::cmp::Ordering::Greater) => (),
                Some(std::cmp::Ordering::Less) => return Ok(Some(Placed::Done)),
                _ => return Ok(Some(Placed::Waits)),
            }
        }
        let Some(count) = self.pending_count(report, &peer, false).await? else {
            return Ok(Some(Placed::Waits));
        };
        if count >= LIMIT {
            let at = key::message_notice(&peer, original);
            if self.kept_notice(&at).await?.is_none() {
                let notice = crate::stored::given_up_notice(
                    message,
                    self.service_time().unwrap_or_default(),
                    crate::notices::GivenUpOrigin::KeptReply,
                );
                effects.put(&at, &notice)?;
            }
            self.local_commit(effects).await?;
            return Ok(Some(Placed::Done));
        }
        if self
            .app
            .scope_key(self.app.data_epoch(), &pending.collection.policy.read_scope)
            .is_none()
        {
            return Ok(Some(Placed::Not));
        }
        let record = PendingAllowanceRecord::new(&own, message.clone())?;
        let mut conditions = vec![absent(a_key)];
        if retained_delete.is_some() {
            conditions.push(self.app.record_condition(&p_key).await?);
        }
        let put = self
            .app
            .record_op(&pending, Some(&record.to_json()?), conditions)
            .await?;
        if retained_delete.is_none() && !is_create(&put.1) {
            return Ok(Some(Placed::Waits));
        }
        // A withdrawal may already be in the inbox when its proposal is first placed.
        // Carry the exact prepared operation on that withdrawal only if the create applies.
        for (_, mut stop) in self
            .local_scan::<KeptMessage>(&key::allowance_ends(&a_key))
            .await?
        {
            let end = stop.stored(&own)?;
            if end.sender() == peer
                && end.message().refs.as_slice() == [message.hash()]
                && stop.position().is_some_and(|at| {
                    self.app.received_order(position, at) == Some(std::cmp::Ordering::Less)
                })
            {
                stop.pending_create = Some(crate::records::PendingCreate {
                    proposal_at: position,
                    operation: prepared_write(
                        own,
                        self.app_id(),
                        self.app.own_identity().generation(),
                        &put.0,
                        &put.1,
                    )?,
                });
                effects.put(&key::allowance_end(&a_key, &end.message().msg_id), &stop)?;
            }
        }
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .entry(put)
            .locals(effects.into_writes());
        Ok(Some(submitted(self.submit(batch).await)?))
    }

    /// A received withdrawal closes only the pending write to which it applies.
    pub(super) async fn remove_withdrawn_proposal(
        &mut self,
        reply: &super::StoredMessage,
        evidence: &KeptMessage,
        kept_key: &str,
    ) -> Result<Option<Placed>> {
        let peer = reply.sender();
        let id = super::named_allowance(reply.body()).ok_or_else(PaykitError::corrupt)?;
        let target = allowance_record_target(Collection::AllowanceProposals, &peer, id);
        let permanent = allowance_target(&peer, id);
        let key = self.app.record_key(&target)?;
        let permanent_key = self.app.record_key(&permanent)?;
        let p_read = self.app.read_record_fresh(&target).await?;
        let a_read = self.app.read_record_fresh(&permanent).await?;
        let (Some(p), Some(a)) = (
            super::delivery::current(&target, &key, &p_read, true),
            super::delivery::current(&permanent, &permanent_key, &a_read, false),
        ) else {
            return Ok(Some(Placed::Waits));
        };
        if a.is_some() || p.is_none() {
            return Ok((a.is_some() && p.is_some()).then_some(Placed::Waits));
        }
        let RecordRead::Fresh(selected) = &p_read else {
            return Ok(Some(Placed::Waits));
        };
        let (proposal, _) = self.selected_pending(selected)?;
        let original = proposal.proposal();
        if proposal.counterparty() != &peer
            || proposal.allowance_id() != id
            || reply.message().refs.as_slice() != [original.hash()]
        {
            return Ok(Some(Placed::Done));
        }
        let Some(held) = self.local_get::<KeptMessage>(kept_key).await? else {
            return Ok(Some(Placed::Done));
        };
        if held.stored(&self.identity())?.hash() != reply.hash()
            || held.position() != evidence.position()
        {
            return Ok(Some(Placed::Waits));
        }
        if reply.body().kind() != Kind::AllowanceEnd
            || original.sender() != peer
            || proposal.conflicted().is_some()
        {
            return Ok(Some(Placed::Done));
        }
        let current = self.app.record_condition(&key).await?;
        if let Some(end) = self
            .local_get::<super::PendingRetirement>(&key::pending_retirement(&key))
            .await?
        {
            match end.compare(evidence.pending_source, evidence.position(), |a, b| {
                self.app.received_order(a, b)
            }) {
                Some(std::cmp::Ordering::Less) => return Ok(Some(Placed::Done)),
                Some(std::cmp::Ordering::Greater) => (),
                _ => return Ok(Some(Placed::Waits)),
            }
        }
        let Some(position) = evidence.position() else {
            return Ok(Some(Placed::Waits));
        };
        if !super::replies::precedes_reply(selected.version, position, |a, b| {
            self.app.received_order(a, b)
        }) {
            let Some(create) = &held.pending_create else {
                return Ok(Some(Placed::Waits));
            };
            if self.app.received_order(create.proposal_at, position)
                != Some(std::cmp::Ordering::Less)
                || self
                    .confirmed_pending_writes(false)
                    .await?
                    .get(&(key, create.operation))
                    .is_none_or(|write| write.version != selected.version)
            {
                return Ok(Some(Placed::Waits));
            }
        }
        if self
            .local_get::<super::PendingConflict>(&key::pending_conflict(&key))
            .await?
            .is_some()
        {
            return Ok(Some(Placed::Done));
        }
        let delete = self
            .app
            .record_op(&target, None, vec![current, absent(permanent_key)])
            .await?;
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .entry(delete)
            .local(LocalWrite::Delete {
                key: kept_key.as_bytes().to_vec(),
            });
        Ok(Some(submitted(self.submit(batch).await)?))
    }
}
