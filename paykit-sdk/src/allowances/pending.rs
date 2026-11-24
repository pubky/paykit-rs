//! Admission, views and replies to current pending Allowance proposals.

use super::{
    allowance_record_target, allowance_target, application, AllowanceId, AllowanceRecord,
    AllowanceStatus, AllowanceView, Body, Paykit, PendingConflict, StoredMessage,
};
use crate::allowances::{AllowanceReply, AppliedReply, EndCause, EndOrigin};
use crate::error::{PaykitError, Refusal, Result};
use crate::records::KeptMessage;
use crate::state::{self, key};
use crate::stored::{submitted, BoundNote, Placed, StoredNote};
use paykit_lib::collections::Collection;
use paykit_lib::kinds::Kind;
use paykit_lib::records::PendingAllowanceRecord;
use paykit_lib::{allowances::Ledger, bodies::ProposerRole};
use pubky_switch_client::app::{
    AppError, KeptReason, LocalWrite, OpBatch, RecordContent, RecordRead, RecordTarget,
    SharedRecord,
};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::ops::{Condition, OpKind, RecordVersion};
use pubky_switch_core::sealed::SealedValue;
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
    /// Commitment to every ordered operation, including its seal and application conditions.
    #[serde(with = "serde_b64")]
    pub(crate) application: [u8; 32],
    pub(crate) source: crate::divergent::SeenVersion,
    /// Only the Switch may carry an unanswered submission across a provider change.
    pub(crate) prepared_in: u64,
    pub(crate) seals: Vec<SealedValue>,
    /// The received acceptance authorizing this generated pre-loss withdrawal.
    pub(crate) fenced_acceptance: Option<KeptMessage>,
}

impl PreparedPendingReply {
    pub(super) fn fenced_reply(&self, own: &[u8; 32]) -> Result<Option<[u8; 32]>> {
        self.fenced_acceptance
            .as_ref()
            .map(|q| q.stored(own).map(|q| q.hash()))
            .transpose()
    }

    pub(super) fn end_cause(&self) -> EndCause {
        if self.fenced_acceptance.is_some() {
            EndCause::ProviderLoss
        } else {
            EndCause::User
        }
    }
}

/// An unplaced signed proposal still belongs to the current pending lifetime
/// unless a verified deletion retired it. Its receipt remains owed until the
/// account record itself reflects it (Paykit spec 8.3.1).
pub(super) fn awaits_competing_proposal(
    own: &[u8; 32],
    pending: &PendingAllowanceRecord,
    end: Option<&super::PendingRetirement>,
    kept: Vec<(String, KeptMessage)>,
    notes: Vec<(String, BoundNote)>,
) -> Result<bool> {
    let competes = |held: &KeptMessage, message: &StoredMessage| {
        message.sender() == *pending.counterparty()
            && matches!(message.body(), paykit_lib::Body::AllowanceProposal(p) if &p.allowance_id == pending.allowance_id())
            && (held.pending_source.is_some() || message.hash() != pending.proposal().hash())
            && !end.is_some_and(|end| {
                end.compare(held.pending_source, held.position(), |a, b| {
                    (a.generation == b.generation).then(|| a.seq.cmp(&b.seq))
                }) == Some(std::cmp::Ordering::Less)
            })
    };
    for (_, held) in kept {
        if competes(&held, &held.stored(own)?) {
            return Ok(true);
        }
    }
    Ok(notes
        .into_iter()
        .any(|(_, bound)| !bound.note.placed && competes(&bound.note.message, &bound.message)))
}

pub(super) enum Acceptance {
    Eligible { ledger: Option<RecordTarget> },
    BeforeLoss,
    MissingExecutionScope,
    Unconfirmed,
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

    pub(super) async fn acceptance(
        &mut self,
        p: &StoredMessage,
        q: &StoredMessage,
        peer: &[u8; 32],
        id: &AllowanceId,
    ) -> Result<Acceptance> {
        let Body::AllowanceProposal(proposal) = p.body() else {
            return Err(PaykitError::corrupt());
        };
        let allower = match proposal.proposer_role {
            ProposerRole::Allower => p.sender() == self.identity(),
            ProposerRole::Allowee => p.message().to == self.identity(),
        };
        if !allower {
            return Ok(Acceptance::Eligible { ledger: None });
        }
        let approving = match proposal.proposer_role {
            ProposerRole::Allower => p,
            ProposerRole::Allowee => q,
        };
        if approving.sender() != self.identity() || approving.cert().app_id() != self.app_id() {
            return Ok(Acceptance::Unconfirmed);
        }
        if !self.holds("paykit_allowance_execute") {
            return Ok(Acceptance::MissingExecutionScope);
        }
        let doc = self.app.own_identity();
        let generation = proposal.allower_generation;
        if doc
            .body()
            .recovery_from
            .is_some_and(|lost| generation <= lost)
        {
            return Ok(Acceptance::BeforeLoss);
        }
        if generation > doc.generation() {
            return Ok(Acceptance::Unconfirmed);
        }
        let ledger = allowance_record_target(Collection::AllowanceLedgers, peer, id);
        if !matches!(
            self.app.read_record_fresh(&ledger).await?,
            RecordRead::Kept {
                reason: KeptReason::NotFound,
                record: None
            }
        ) {
            return Ok(Acceptance::Unconfirmed);
        }
        Ok(Acceptance::Eligible {
            ledger: Some(ledger),
        })
    }

    pub(super) async fn acceptance_records(
        &mut self,
        p: &StoredMessage,
        q: &StoredMessage,
        peer: &[u8; 32],
        id: &AllowanceId,
        ledger: Option<RecordTarget>,
    ) -> Result<Option<Vec<application::RecordWrite>>> {
        let permanent = allowance_target(peer, id);
        let accepted = AllowanceRecord {
            counterparty: *peer,
            allowance_id: id.clone(),
            proposal: p.clone(),
            response: Some(q.clone()),
            end: None,
            conflicted: None,
            grant: None,
            grants: Vec::new(),
            ledger_handoff: None,
            ledger_handoff_unread: None,
            msg_ids: Vec::new(),
            unknown: Default::default(),
        };
        let put = self
            .app
            .record_op(&permanent, Some(&accepted.to_json()?), Vec::new())
            .await?;
        if !is_create(&put.1) {
            return Ok(None);
        }
        let mut operations = vec![put];
        if let Some(ledger) = ledger {
            let value = Ledger::initial_value(peer, id, &q.hash());
            let put = self
                .app
                .record_op(&ledger, Some(&value), Vec::new())
                .await?;
            if !is_create(&put.1) {
                return Ok(None);
            }
            operations.push(put);
        }
        Ok(Some(operations))
    }

    pub(super) fn selected_pending(
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

    pub(super) async fn competing_proposal_owed(
        &self,
        pending: &PendingAllowanceRecord,
        pending_key: &[u8; 32],
        permanent_key: &[u8; 32],
    ) -> Result<bool> {
        if self.unresolved_allowance(pending_key).await?
            || self.unresolved_allowance(permanent_key).await?
        {
            return Ok(true);
        }
        let peer = pending.counterparty();
        let end = self
            .local_get::<super::PendingRetirement>(&key::pending_retirement(pending_key))
            .await?;
        let kept = self
            .local_scan(&key::kept_reply(peer, permanent_key, ""))
            .await?;
        let notes = self.local_scan(&key::stored_notes_of(peer)).await?;
        awaits_competing_proposal(&self.identity(), pending, end.as_ref(), kept, notes)
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
        for (pending_key, (pending, conflicted)) in by_key {
            let permanent = self.app.record_key(&allowance_target(
                pending.counterparty(),
                pending.allowance_id(),
            ))?;
            if wanted.is_some_and(|key| *key != permanent)
                || self.app.shared_record(&permanent).await?.is_some()
            {
                continue;
            }
            let status = if !conflicted
                && (self.unresolved_allowance(&pending_key).await?
                    || self.unresolved_allowance(&permanent).await?)
            {
                AllowanceStatus::AwaitingConflictCheck
            } else if conflicted
                || self.conflict_kept(&permanent).await?
                || self
                    .competing_proposal_owed(&pending, &pending_key, &permanent)
                    .await?
            {
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

    pub(super) async fn confirmed_pending_applications(
        &self,
    ) -> Result<application::AppliedApplications> {
        application::applied_applications(
            self.app.shared_evidence().await?.ops,
            self.app.own_identity(),
            self.app_id(),
        )
    }

    /// Restore a prepared reply only from the signed operation and the complete service
    /// result for its exact delete. A retained tombstone or a hosted absence alone cannot
    /// tell whether this app's write applied.
    pub(crate) async fn recover_pending_replies(&mut self) -> Result<()> {
        let intents = self
            .local_scan::<PreparedPendingReply>(key::PENDING_REPLIES)
            .await?;
        self.recover_pending_applications(intents, true).await
    }

    /// Reopen from the local store alone. An incoming acceptance has no outgoing
    /// reply intent, but the payer still needs the applied batch before a grant.
    pub(crate) async fn recover_original_approvals(&mut self) -> Result<()> {
        self.recover_pending_applications(Vec::new(), false).await
    }

    async fn recover_pending_applications(
        &mut self,
        intents: Vec<(String, PreparedPendingReply)>,
        allow_delivery: bool,
    ) -> Result<()> {
        let mut approvals = Vec::new();
        let mut withdrawals = Vec::new();
        for (record, approval) in self
            .local_scan::<super::OriginalAllowance>(key::ORIGINAL_ALLOWANCES)
            .await?
        {
            if let Some(kept) = approval.received_loss_return().filter(|_| allow_delivery) {
                Box::pin(self.resume_loss_return(kept)).await?;
            } else if approval.awaiting_application()
                || approval.awaiting_original_create(self.app.own_identity().generation())
            {
                approvals.push((record, approval));
            } else if allow_delivery && approval.withdrawal_owed() {
                withdrawals.push((record, approval));
            }
        }
        for (record, approval) in withdrawals {
            let at = format!("{}{record}", key::ORIGINAL_ALLOWANCES);
            self.withdraw_rejected_proposal(&at, approval).await?;
        }
        if intents.is_empty() && approvals.is_empty() {
            return Ok(());
        }
        let confirmed = self.confirmed_pending_applications().await?;
        for (record, mut approval) in approvals {
            let at = format!("{}{record}", key::ORIGINAL_ALLOWANCES);
            if approval.awaiting_original_create(self.app.own_identity().generation()) {
                self.recover_original_create(&at, &mut approval, &confirmed)
                    .await?;
            }
            if self
                .capture_original_approval(&at, &mut approval, &confirmed)
                .await?
                && allow_delivery
                && approval.withdrawal_owed()
            {
                self.withdraw_rejected_proposal(&at, approval).await?;
            }
        }
        for (at, intent) in intents {
            let msg = intent.message.stored(&intent.peer)?;
            let id = super::named_allowance(msg.body()).ok_or_else(PaykitError::corrupt)?;
            let pending = allowance_record_target(Collection::AllowanceProposals, &intent.peer, id);
            let pending_key = self.app.record_key(&pending)?;
            let applied = confirmed
                .replies
                .get(&(msg.cert().signing_key(), intent.application))
                .filter(|batch| batch.pending_key == pending_key);
            let saved = if applied.is_none() && intent.fenced_acceptance.is_some() {
                let record = self.app.record_key(&allowance_target(&intent.peer, id))?;
                match self
                    .local_get::<super::OriginalAllowance>(&key::original_allowance(&record))
                    .await?
                {
                    Some(original) => {
                        original.saved_loss_application(pending_key, &msg, intent.application)?
                    }
                    None => None,
                }
            } else {
                None
            };
            let applied = applied.or(saved.as_ref());
            let mut writes = vec![LocalWrite::Delete {
                key: format!("{}{at}", key::PENDING_REPLIES).into_bytes(),
            }];
            let queued = if let Some(batch) = applied {
                if batch.members.len() == 3 {
                    let approval = self.app.record_key(&allowance_target(&intent.peer, id))?;
                    if !self
                        .local_get::<super::OriginalAllowance>(&key::original_allowance(&approval))
                        .await?
                        .is_some_and(|a| a.retains_application(&intent.application, &batch.members))
                    {
                        continue;
                    }
                }
                if intent.fenced_acceptance.is_some() {
                    let record = self.app.record_key(&allowance_target(&intent.peer, id))?;
                    let Some(protected) =
                        self.capture_loss_deletion(&record, &intent, batch).await?
                    else {
                        continue;
                    };
                    self.app
                        .protocol_commit(crate::protocol::BACKUP_NAME, &[protected])
                        .await?;
                }
                // Queue first. A stop before the local release still leaves the identical
                // signed original recoverable from the applied delete.
                match self.app.queue_stored(msg.plaintext()).await {
                    Ok(id) => Some((id, batch.retirement)),
                    Err(pubky_switch_client::app::AppError::Refused(
                        pubky_switch_client::app::Refusal::WontDeliver(_),
                    )) => None,
                    Err(error) => return Err(error.into()),
                }
            } else {
                if intent.fenced_acceptance.is_some() || !self.past_replay_window(&msg) {
                    continue;
                }
                match self.app.delivery(&msg.message().msg_id).await? {
                    Some(pubky_switch_client::app::Delivery::Accepted { .. }) => {
                        self.app
                            .protocol_commit(crate::protocol::BACKUP_NAME, &writes)
                            .await?;
                        continue;
                    }
                    Some(
                        pubky_switch_client::app::Delivery::Kept { .. }
                        | pubky_switch_client::app::Delivery::Queued { .. },
                    ) => continue,
                    _ => None,
                }
            };
            let Some((msg_id, version)) = queued else {
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
            };
            writes.push(state::local_put(
                &key::applied_reply(&msg.hash()),
                &AppliedReply::new(&msg, version),
            )?);
            self.app
                .protocol_commit(crate::protocol::BACKUP_NAME, &writes)
                .await?;
            if allow_delivery {
                Box::pin(self.delivered(&msg_id)).await?;
            }
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
        let mut active = None;
        for (_, intent) in self
            .local_scan::<PreparedPendingReply>(&key::pending_replies(key))
            .await?
        {
            let signed = intent.message.stored(&intent.peer)?;
            let id = super::named_allowance(signed.body()).ok_or_else(PaykitError::corrupt)?;
            let target = allowance_record_target(Collection::AllowanceProposals, &intent.peer, id);
            let pending = self.app.record_key(&target)?;
            if self
                .local_get::<super::PendingRetirement>(&key::pending_retirement(&pending))
                .await?
                .is_some_and(|end| end.retires(intent.source))
            {
                continue;
            }
            if self.unresolved_allowance(&pending).await? || self.unresolved_allowance(key).await? {
                return Err(not_current());
            }
            if active.replace(intent).is_some() {
                return Err(not_current());
            }
        }
        let Some(intent) = active else {
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
            None => {
                Box::pin(self.retry_prepared_answer(key, &intent)).await?;
                Ok(Some(msg_id.clone()))
            }
            _ => Err(Refusal::NotNow(
                "the original Allowance answer has no verified outcome".into(),
            )
            .into()),
        }
    }

    /// Resume a signed original that has no outbox item. A successful delete is reconciled
    /// first. Otherwise only the exact sealed write against the same application may run.
    async fn retry_prepared_answer(
        &mut self,
        key: &[u8; 32],
        intent: &PreparedPendingReply,
    ) -> Result<()> {
        Box::pin(self.recover_pending_replies()).await?;
        let original = intent.message.stored(&intent.peer)?;
        if self
            .app
            .delivery(&original.message().msg_id)
            .await?
            .is_some()
        {
            return Ok(());
        }
        // Reconciliation can finish an undeliverable original and record its give-up.
        // Such an answer cannot be submitted again, even if the proposal reappears.
        if self
            .local_get::<PreparedPendingReply>(&key::pending_reply(key, &original.hash()))
            .await?
            .is_none()
        {
            return Err(not_current());
        }
        let Some((selected, _)) = self.pending_by_allowance_key(key).await? else {
            return Err(not_current());
        };
        let kind = original.body().kind();
        let (pending, permanent, mut delete) =
            Box::pin(self.checked_pending_answer(&selected, kind)).await?;
        if permanent != *key
            || self.app.own_identity().generation() != intent.prepared_in
            || selected.version != intent.source.record_version()
            || original.message().refs != [pending.proposal().hash()]
            || original.cert().signing_key() != self.app.cert().signing_key()
        {
            return Err(not_current());
        }
        if let Some(q) = &intent.fenced_acceptance {
            let reply = q.stored(&self.identity())?;
            if !self.allowance_end_send_permitted(&intent.peer).await? {
                return Err(not_current());
            }
            let selected =
                Box::pin(self.live_original_after_loss(&selected, pending.proposal(), &reply))
                    .await?
                    .ok_or_else(not_current)?;
            selected.bind_deletion(&mut delete)?;
        }
        let mut operations = self.answer_writes(&pending, &original, delete).await?;
        let mut retained = state::Tx::default();
        Box::pin(self.keep_original_selection(
            &selected,
            pending.proposal(),
            &original,
            &mut operations,
            None,
            &mut retained,
        ))
        .await?;
        self.local_commit(retained).await?;
        application::restore_seals(
            &self.app,
            &mut operations,
            &intent.seals,
            intent.application,
        )?;
        let mut batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME);
        for op in operations {
            batch = batch.entry(op);
        }
        match self.submit(batch).await {
            Ok(_) => Box::pin(self.recover_pending_replies()).await,
            Err(error) if error.kept_batch().is_some() => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Select the original pending write and recheck every condition that authorizes this answer.
    async fn checked_pending_answer(
        &mut self,
        selected: &SharedRecord,
        kind: Kind,
    ) -> Result<(PendingAllowanceRecord, [u8; 32], (Vec<Condition>, OpKind))> {
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
            || self
                .competing_proposal_owed(&pending, &selected.key, &permanent_key)
                .await?
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
        if (kind == Kind::AllowanceEnd) != (p.sender() == self.identity()) {
            return Err(Refusal::Usage(
                "only the proposer withdraws a pending Allowance and only its recipient answers it"
                    .into(),
            )
            .into());
        }
        if kind != Kind::AllowanceEnd {
            self.refuse_while_sends_held(peer).await?;
        }
        let mut conditions = vec![condition];
        if kind == Kind::AllowanceAcceptance {
            self.refuse_while_paused(&[selected.key]).await?;
        } else {
            conditions.push(absent(permanent_key));
        }
        // The same guarded write serves a fresh decision and its exact signed retry.
        let delete = self.app.record_op(&target, None, conditions).await?;
        Ok((pending, permanent_key, delete))
    }

    async fn answer_writes(
        &mut self,
        pending: &PendingAllowanceRecord,
        reply: &StoredMessage,
        delete: application::RecordWrite,
    ) -> Result<Vec<application::RecordWrite>> {
        let mut operations = Vec::new();
        if reply.body().kind() == Kind::AllowanceAcceptance {
            let p = pending.proposal();
            let peer = pending.counterparty();
            let id = pending.allowance_id();
            let ledger = match self.acceptance(p, reply, peer, id).await? {
                Acceptance::Eligible { ledger } => ledger,
                Acceptance::BeforeLoss => {
                    return Err(Refusal::NotNow(
                        "the proposal predates the Allower's provider loss".into(),
                    )
                    .into());
                }
                Acceptance::MissingExecutionScope => {
                    return Err(Refusal::ScopeMissing("paykit_allowance_execute".into()).into());
                }
                Acceptance::Unconfirmed => return Err(not_current()),
            };
            operations = self
                .acceptance_records(p, reply, peer, id, ledger)
                .await?
                .ok_or_else(not_current)?;
        }
        operations.push(delete);
        Ok(operations)
    }

    /// Only the identical generated withdrawal may resume an already prepared decision.
    pub(super) async fn withdraw_fenced_pending(
        &mut self,
        selected: &SharedRecord,
        record: &[u8; 32],
        acceptance: &StoredMessage,
        original: super::original::LiveOriginal,
    ) -> Result<bool> {
        let intents = self
            .local_scan::<PreparedPendingReply>(&key::pending_replies(record))
            .await?;
        let id = match intents.as_slice() {
            [] => Some(
                Box::pin(self.answer_pending(
                    selected,
                    Kind::AllowanceEnd,
                    None,
                    Some((acceptance, original)),
                ))
                .await?,
            ),
            [(_, intent)] if intent.fenced_reply(&self.identity())? == Some(acceptance.hash()) => {
                Box::pin(self.resume_pending_answer(record, Kind::AllowanceEnd)).await?
            }
            _ => None,
        };
        let Some(id) = id else {
            return Ok(false);
        };
        let Some(origin) = self.local_get::<EndOrigin>(&key::end_origin(&id)).await? else {
            return Ok(false);
        };
        Ok(self
            .local_get::<AppliedReply>(&key::applied_reply(&origin.message_hash))
            .await?
            .is_some())
    }

    /// Open only the current pending value at the user's selected key, followed by its
    /// permanent record. The cached value supplies the natural key for the first read.
    pub(super) async fn answer_pending(
        &mut self,
        selected: &SharedRecord,
        kind: Kind,
        execution: Option<crate::UserConfirmed>,
        fenced: Option<(&StoredMessage, super::original::LiveOriginal)>,
    ) -> Result<String> {
        let own = self.identity();
        let (pending, permanent_key, mut delete) =
            Box::pin(self.checked_pending_answer(selected, kind)).await?;
        if let Some((_, original)) = &fenced {
            original.bind_deletion(&mut delete)?;
        }
        let p = pending.proposal();
        let peer = pending.counterparty();
        let id = pending.allowance_id();
        if kind == Kind::AllowanceAcceptance {
            let Body::AllowanceProposal(proposal) = p.body() else {
                return Err(PaykitError::corrupt());
            };
            if proposal.proposer_role == ProposerRole::Allowee {
                self.require_payer_choice(execution.as_ref())?;
            } else if execution.is_some() {
                return Err(
                    Refusal::Usage("only the payer can approve automatic payment".into()).into(),
                );
            }
        }
        let body = AllowanceReply {
            allowance_id: id.clone(),
            executor_app_id: None,
        }
        .to_body(kind)?;
        let reply = self.sign(*peer, kind, vec![p.hash()], body)?;
        let q = self.own_copy(&reply)?;
        let intent_key = key::pending_reply(&permanent_key, &q.hash());
        let mut operations = self.answer_writes(&pending, &q, delete).await?;
        let mut prepare = state::Tx::default();
        Box::pin(self.keep_original_selection(
            selected,
            p,
            &q,
            &mut operations,
            Some(execution.is_some()),
            &mut prepare,
        ))
        .await?;
        let application = application::prepared_application(
            own,
            self.app_id(),
            self.app.own_identity().generation(),
            &operations,
        )?;
        let seals = application::seals(&operations)?;
        let mut batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME);
        for operation in operations {
            batch = batch.entry(operation);
        }
        let msg_id = reply.msg_id().to_owned();
        prepare.put(
            &intent_key,
            &PreparedPendingReply {
                peer: *peer,
                message: KeptMessage::of(&q),
                application,
                source: selected.version.into(),
                prepared_in: self.app.own_identity().generation(),
                fenced_acceptance: fenced.as_ref().map(|(q, _)| KeptMessage::of(q)),
                seals,
            },
        )?;
        let is_fenced = fenced.is_some();
        if let Some((acceptance, create)) = fenced {
            Box::pin(self.prepare_loss_settlement(
                &permanent_key,
                create,
                acceptance,
                &q,
                application,
                &mut prepare,
            ))
            .await?;
        }
        if kind == Kind::AllowanceEnd {
            prepare.put(
                &key::end_origin(&msg_id),
                &EndOrigin::of(
                    permanent_key,
                    p,
                    None,
                    &q,
                    if is_fenced {
                        EndCause::ProviderLoss
                    } else {
                        EndCause::User
                    },
                )?,
            )?;
        }
        self.local_commit(prepare).await?;
        let batch = if is_fenced { batch } else { batch.queue(reply) };
        match Box::pin(self.submit(batch)).await {
            Ok(submitted) => {
                if is_fenced {
                    let intent = self
                        .local_get::<PreparedPendingReply>(&intent_key)
                        .await?
                        .ok_or_else(PaykitError::corrupt)?;
                    let at = intent_key
                        .strip_prefix(key::PENDING_REPLIES)
                        .ok_or_else(PaykitError::corrupt)?;
                    Box::pin(
                        self.recover_pending_applications(vec![(at.to_owned(), intent)], false),
                    )
                    .await?;
                } else if kind == Kind::AllowanceAcceptance {
                    Box::pin(self.recover_pending_replies()).await?;
                } else {
                    let [proof] = submitted.proofs.as_slice() else {
                        return Err(PaykitError::corrupt());
                    };
                    let at = proof
                        .record_revision()
                        .map_err(|_| PaykitError::corrupt())?;
                    let mut writes = state::Tx::default();
                    writes.put(
                        &key::applied_reply(&q.hash()),
                        &AppliedReply::new(&q, RecordVersion::new(at.generation, at.seq)),
                    )?;
                    writes.delete(&intent_key);
                    self.local_commit(writes).await?;
                }
                if !is_fenced {
                    self.delivered(&msg_id).await?;
                }
            }
            Err(error) if error.kept_batch().is_some() => (),
            Err(error) => {
                // An earlier batch can fail before this one is signed. These local
                // refusals likewise mean it was never kept or sent. An unreadable
                // store leaves its outcome unknown, so the original stays prepared.
                // An acceptance also owns the exact sealed opening and its prior user
                // choice. A later retry must always return to that signed answer.
                if kind != Kind::AllowanceAcceptance
                    && !is_fenced
                    && (matches!(error, AppError::Ahead { .. } | AppError::Refused(_))
                        || error.code() == Some(pubky_switch_core::http::ErrorCode::Conflict))
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
        if let Some(end) = self
            .local_get::<super::PendingRetirement>(&key::pending_retirement(&p_key))
            .await?
        {
            match end.compare(held.pending_source, Some(position), |a, b| {
                self.app.received_order(a, b)
            }) {
                Some(std::cmp::Ordering::Greater) => (),
                Some(std::cmp::Ordering::Less) => return Ok(Some(Placed::Done)),
                _ => return Ok(Some(Placed::Waits)),
            }
        }
        if p.is_some() {
            return self.place_received_second(message, &held, stored_id).await;
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
        // A proposal first received over a live original is owed for that
        // lifetime. Its absence from a new installation cannot admit it again.
        if held.pending_source.is_some() {
            return Ok(Some(Placed::Waits));
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
                    operation: application::prepared_member(
                        own,
                        self.app_id(),
                        self.app.own_identity().generation(),
                        1,
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
        if self
            .competing_proposal_owed(&proposal, &key, &permanent_key)
            .await?
        {
            return Ok(Some(Placed::Waits));
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
                    .confirmed_pending_applications()
                    .await?
                    .pending_creates
                    .get(&(key, create.operation))
                    .is_none_or(|(version, _)| {
                        *version != selected.version
                            || !super::delivery::deletion_continues(
                                version.generation,
                                self.app.own_identity().body().recovery_from,
                            )
                    })
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

    /// Discard a conflicted pending proposal or one received from a blocked counterparty.
    /// This removes exactly the selected pending write and sends no message (Paykit spec 8.3.1).
    ///
    /// # Errors
    ///
    /// A changed proposal or an uncertain read leaves the pending record in place.
    pub async fn discard_pending_allowance(&mut self, record: &[u8; 32]) -> Result<()> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        self.refresh_own()?;
        self.first_read_before_writing().await?;
        self.refresh_own()?;
        self.require_allowance()?;
        self.records_known().await?;
        let (selected, target) = Box::pin(self.pending_by_allowance_key(record))
            .await?
            .ok_or(Refusal::NotFound)?;
        let condition = self.app.record_condition(&selected.key).await?;
        let RecordRead::Fresh(held) = self.app.read_record_fresh(&target).await? else {
            return Err(not_current());
        };
        let (current, _) = self.selected_pending(&held)?;
        if held.content != selected.content
            || self.app.record_condition(&held.key).await? != condition
        {
            return Err(not_current());
        }
        let peer = current.counterparty();
        let a = allowance_target(peer, current.allowance_id());
        if !matches!(
            self.app.read_record_fresh(&a).await?,
            RecordRead::Kept {
                reason: KeptReason::NotFound,
                record: None
            }
        ) {
            return Err(not_current());
        }
        let stop = self
            .local_get::<PendingConflict>(&key::pending_conflict(&held.key))
            .await?;
        let local_conflict = stop
            .as_ref()
            .map(|s| s.stops(&self.identity(), &current))
            .transpose()?
            .unwrap_or(false);
        if current.conflicted().is_none()
            && !local_conflict
            && !self
                .may_discard_with_evidence(&target, &held, &current)
                .await?
            && (current.proposal().sender() != *peer || !self.denied(peer).await?)
        {
            return Err(Refusal::Usage("only a conflicted proposal or a blocked counterparty's received proposal may be discarded".into()).into());
        }
        let conditions = vec![
            condition,
            Condition::RecordVersion {
                key: *record,
                version: RecordVersion::NONE,
            },
        ];
        let delete = self.app.record_op(&target, None, conditions).await?;
        self.submit(OpBatch::for_protocol(crate::protocol::BACKUP_NAME).entry(delete))
            .await?;
        Ok(())
    }
}
