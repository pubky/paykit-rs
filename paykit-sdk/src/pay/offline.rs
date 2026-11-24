//! A manually selected public endpoint while the payer's switch is unavailable
//! (Paykit spec 8.4). Only this entry point calls the payment method.

use super::*;
use crate::journal::OfflineRecording;
use pubky_switch_client::app::{server_time_in, KeptBatchDrop, ProviderStatus};

impl Paykit {
    pub(super) async fn pay_public_offline(
        &mut self,
        mut entry: JournalEntry,
        carrier: Carrier,
        options: &PayOptions,
    ) -> Result<PayOutcome> {
        self.own_identity_confirmed(true).await??;
        if self.app.follow_provider().await? != ProviderStatus::Current {
            return Err(
                Refusal::NotNow("follow the payer's current switch before paying".into()).into(),
            );
        }
        let mut peer = self
            .check_destination(
                &entry.payee,
                &entry.destination,
                &carrier,
                None,
                Initiator::User,
                options.unconfirmed_identity_confirmed,
            )
            .await??;
        entry.offline = Some(OfflineRecording::AwaitingRecording);
        entry.keep_carrier(&carrier);
        self.save(&entry).await?;
        self.set_stage(&mut entry, Stage::Paying).await?;
        if let Err(error) = self
            .offline_dispatch_check(&entry, &mut peer, &carrier, options)
            .await
        {
            self.set_stage(&mut entry, Stage::Abandoned).await?;
            return Err(error);
        }
        let attempt_id = entry.attempt()?;
        let payment = RailPayment {
            endpoint_id: entry.destination.endpoint_id()?,
            payload: entry.destination.payload()?,
            amount: entry.call_amount()?,
            idempotency_key: attempt_id,
            payment_reference: None,
        };
        match self.rail.pay(payment).await {
            Ok(outcome) => self.take_outcome(&mut entry, outcome, None).await,
            Err(RailError::Refused(_)) => {
                self.take_outcome(&mut entry, RailOutcome::Failed, None)
                    .await
            }
            Err(_) => Ok(PayOutcome::Uncertain { attempt_id }),
        }
    }

    async fn offline_dispatch_check(
        &mut self,
        entry: &JournalEntry,
        peer: &mut PeerIdentity,
        carrier: &Carrier,
        options: &PayOptions,
    ) -> Result<()> {
        if let Some(answer) = self.take_kept_lookup(&entry.payee).await?? {
            *peer = answer;
        }
        let read = self.local_get::<u64>(key::LISTS_READ).await?;
        let copy = self.local_get::<Vec<[u8; 32]>>(key::DENIED).await?;
        let mark = self
            .local_get::<state::OwnBlockMark>(&key::own_block(&entry.payee))
            .await?;
        let changed = match mark {
            Some(mark) => Some(self.own_change(&mark).await?),
            None => None,
        };
        let lookup_answered = self.inspect_ready_lookup(&entry.payee, |_| ()).is_some();
        self.require_pay()?;
        self.transfer_holds(entry, peer, false)??;
        self.app.may_attempt(false).map_err(held_back)?;
        let (now, own_fresh) = self.dispatch_time();
        let Some((_, denied)) = read
            .zip(copy)
            .filter(|(at, _)| crate::receive::lists_read_is_fresh(*at, now))
        else {
            return Err(
                Refusal::SwitchUnavailable("the current deny list is unavailable".into()).into(),
            );
        };
        if changed == Some(None) {
            return Err(Refusal::NotNow("the deny list must be read again".into()).into());
        }
        if changed
            .flatten()
            .unwrap_or_else(|| denied.contains(&entry.payee))
        {
            let amount = entry.paid_amount()?.ok_or_else(|| {
                Refusal::NotNow("the blocked payment's transfer amount is unknown".into())
            })?;
            refuse_blocked(
                true,
                BlockedPayment {
                    payee: entry.payee,
                    request: None,
                    period: None,
                    amount,
                },
                options.blocked_confirmed.as_ref(),
            )?;
        }
        if !own_fresh {
            return Err(Refusal::IdentityUnavailable(
                "the payer's identity must be confirmed again".into(),
            )
            .into());
        }
        self.own_identity_lists_app(now)
            .map_err(Refusal::NotCurrent)?;
        self.destination_valid(
            &entry.payee,
            &entry.destination,
            carrier,
            None,
            &peer.doc,
            now,
        )?;
        if lookup_answered
            || !self.payee_source_current(
                carrier,
                peer,
                Initiator::User,
                options.unconfirmed_identity_confirmed,
                now,
            )
        {
            return Err(Refusal::IdentityUnavailable(
                "the payee's identity must be confirmed again".into(),
            )
            .into());
        }
        Ok(())
    }

    pub(super) async fn offline_method_outcome(
        &mut self,
        entry: &mut JournalEntry,
    ) -> Result<PayOutcome> {
        let attempt_id = entry.attempt()?;
        // Silence on the claim or in a device-local wallet is no evidence of nonpayment.
        let declaration = self.rail.declaration();
        match self.rail.payment(attempt_id).await? {
            Some(outcome) => self.take_outcome(entry, outcome, None).await,
            None if declaration.history
                && (!entry.restored || declaration.history_survives_restore)
                && retained(
                    declaration.history_retention_ms,
                    entry.journaled_at_ms,
                    self.now(),
                ) =>
            {
                self.take_outcome(entry, RailOutcome::Failed, None).await
            }
            None => Ok(PayOutcome::Uncertain { attempt_id }),
        }
    }

    pub(super) async fn record_offline_attempt(
        &mut self,
        entry: &mut JournalEntry,
    ) -> Result<bool> {
        let attempt = entry.attempt()?;
        let marker = key::offline_batch(&attempt);
        let claim = self.app.read_claim(&entry.obligation).await?;
        let target = ActivityTarget::payer(entry.natural_key.clone());
        // A checked missing-claim answer can outlive the service's retention. A
        // previously checked view may still establish that the original attempt
        // applied. Its old step cannot authorize a new entry on a forgotten claim.
        let held = if claim.is_none() {
            self.app.claim_evidence(&entry.obligation).await?
        } else {
            None
        };
        let view = claim
            .as_ref()
            .map(|read| &read.view)
            .or_else(|| held.as_ref().map(|evidence| &evidence.view));
        if let Some((view, shown)) =
            view.and_then(|view| view.attempt(&attempt.to_bytes()).map(|shown| (view, shown)))
        {
            if shown.owner != self.app_id() {
                return Ok(false);
            }
            self.app.read_record(&target.record_target()).await?;
            entry.deadline = view.deadline;
            entry.step = (claim.is_some()
                && matches!(shown.outcome, Outcome::Open | Outcome::Unknown))
            .then_some(view.step);
            entry.failed_on_claim = shown.outcome == Outcome::Failed;
            entry.offline = Some(OfflineRecording::Applied);
            let mut tx = state::Tx::default();
            entry.check_bound(|input| self.app.blind(input))?;
            tx.put(&key::journal(&attempt), entry)?.delete(&marker);
            self.local_commit(tx).await?;
            return Ok(true);
        }
        if matches!(entry.offline, Some(OfflineRecording::Applied)) {
            // The client's applied-only local write survives even if the claim was forgotten.
            entry.step = None;
            return Ok(true);
        }
        if entry.restored || claim.is_some() || self.app.reconcile_mode() {
            return Ok(false);
        }
        if let Some(id) = self.local_get::<[u8; 16]>(&marker).await? {
            let original = ClaimAttempt {
                claim_key: entry.obligation,
                attempt_id: attempt.to_bytes(),
            };
            // A marker outlives the client's status retention. Only a definite drop of
            // this same attempt and batch permits another recording (Paykit spec 8.4).
            match self.app.kept_attempt(original).await? {
                Some(KeptBatch {
                    batch_id,
                    status:
                        KeptBatchStatus::Dropped {
                            reason: KeptBatchDrop::Unsent | KeptBatchDrop::Refused { .. },
                            ..
                        },
                }) if batch_id == id => self.local_delete(&marker).await?,
                _ => return Ok(false),
            }
        }
        // Read the payer record before creating it. Existing or unreadable data cannot
        // establish that a previous recording never applied.
        if self
            .app
            .read_record(&target.record_target())
            .await?
            .is_some()
        {
            return Ok(false);
        }
        let seen = self
            .service_time()
            .ok_or_else(|| Refusal::NotNow("no verified service time".into()))?;
        let deadline = seen
            .checked_add(UNREQUESTED_DEADLINE_MS)
            .ok_or_else(PaykitError::corrupt)?;
        let orphan_after_s = self.config.orphan_after_s_for(&self.rail.declaration());
        let mut record = activity::empty(ActivitySide::Payer, entry.payee);
        record.attempt_id = Some(attempt);
        record.unrequested = Some(crate::receive::unrequested_of(
            &entry.destination,
            &entry.amount.amount()?,
        )?);
        record.status = Some("paying".into());
        let value = record.to_json()?;
        let write = self
            .app
            .record_op(&target.record_target(), Some(&value), Vec::new())
            .await?;
        if !matches!(&write.1, OpKind::RecordPut(put) if put.expected_version.is_none()) {
            return Ok(false);
        }
        let mut applied = entry.clone();
        applied.deadline = Some(deadline);
        applied.offline = Some(OfflineRecording::Applied);
        let op = ClaimOp::attempt(
            entry.obligation,
            0,
            attempt.to_bytes(),
            orphan_after_s,
            Some(deadline),
        );
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .op_if(op.kind(), vec![server_time_in(None, Some(deadline))])
            .first_send_before(deadline)
            .obligation_deadline(entry.obligation, ObligationDeadline::At(deadline))
            .entry(write)
            // The original ID and signed operations become durable together (Paykit spec 8.4).
            .keep_id_at(marker.as_bytes().to_vec())
            .local(self.journal_write(&applied)?)
            .local(LocalWrite::Delete {
                key: marker.into_bytes(),
            });
        let submitted = self.app.submit_batch(batch).await?;
        let proof = submitted.proofs.first().ok_or_else(PaykitError::corrupt)?;
        applied.step = proof.body().step;
        applied.attempt_proof = Some(proof.signed().clone());
        self.save(&applied).await?;
        *entry = applied;
        Ok(true)
    }
}
