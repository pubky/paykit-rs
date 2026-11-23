//! The first executor grant, conditioned on the original permanent ledger (Paykit spec 8.3).

use paykit_lib::grant::{grant_fields, EXECUTOR_GRANT_LABEL};
use paykit_lib::records::{AllowanceRecord, GrantEntry, Origin};
use paykit_lib::{Body, ExecutorGrant};
use pubky_switch_client::app::{
    KeptBatchStatus, KeptOp, LocalWrite, OpBatch, RecordRead, RecordTarget, UserConfirmed,
};
use pubky_switch_core::enc::hash;
use pubky_switch_core::ops::{Condition, OpKind};

use super::approval::ApprovedOpening;
use super::checkpoint::{ledger_target, ExecutorCheckpoint, ExecutorHistory, GrantEvidence};
use super::evidence::AppliedRecord;
use super::spend::{standing, Standing};
use crate::allowances::{allowance_target, Consent, ConsentEntry};
use crate::state::{self, key, Tx};
use crate::{AllowanceRole, AllowanceStatus, Paykit, Refusal, Result};

/// Why the regrant after a provider loss waits: the Allowance record isn't read as writing
/// on it needs, as after an unverified or not yet taken write.
const REGRANT_WAITS: &str = "the Allowance record isn't read as writing on it needs yet";

fn regrant_waits() -> Refusal {
    Refusal::NotNow(REGRANT_WAITS.into())
}

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
    /// including after reopening, and record the consent once the grant has applied. A grant
    /// whose answer was lost is sent again first, so the host retries this call until it
    /// succeeds, with no read needed in between. An independent later hold requires its own
    /// confirmation. A repeated call requires the same app's current grant and the original
    /// or reconciled ledger.
    ///
    /// # Errors
    ///
    /// Where the Allowance stands first, as a payment under it gets it: for good
    /// [`Refusal::AllowanceEnded`], [`Refusal::AllowanceConflicted`], [`Refusal::Rejected`],
    /// [`Refusal::Usage`] or [`Refusal::NotAutomatic`]; [`Refusal::ScopeMissing`] until the
    /// signer renews this app's certificate with the scope; [`Refusal::AllowanceConfirmationNeeded`]
    /// when the user must confirm it again ([`Paykit::confirm_allowance_executor`]); and
    /// [`Refusal::NotNow`] when the original approval, its complete service application or
    /// current authority is unavailable, or the ledger cannot be reconciled yet. Accessing the
    /// store or service can fail too.
    pub async fn resume_allowance_executor(&mut self, record: &[u8; 32]) -> Result<()> {
        Box::pin(self.apply_executor_grant(record, None)).await
    }

    /// Confirm this app's previously approved execution again after a consent hold.
    /// The host shows the counterparty, terms and executor. An earlier approval of
    /// automatic execution by this app is required (Paykit spec 8.3). After a provider loss,
    /// for an Allowance the user confirmed before it, this writes its grant again at the
    /// current generation; automatic payments under it don't resume in this version, which
    /// doesn't repair its usage (Paykit spec 8.3, "After provider loss").
    /// This app can renew held consent against its own already incorporated ledger. A
    /// restored or unrecognized later ledger requires complete wallet history.
    ///
    /// # Errors
    ///
    /// Where the Allowance stands, as [`Paykit::resume_allowance_executor`] says. The
    /// confirmation is the user's for `hold`, the Allowance and the hold in force when it was
    /// shown ([`crate::AllowanceView::hold`], [`Refusal::AllowanceConfirmationNeeded`]): once
    /// a newer restore, `missed` item or provider loss has set another hold, before this call
    /// or during its
    /// read, it is refused with [`Refusal::AllowanceConfirmationNeeded`] naming the hold in
    /// force now, nothing is written, and the host asks the user again. It can still refuse
    /// with [`Refusal::NotNow`] when the original ledger or grant result can't be established
    /// yet, for example while the payment method's history is incomplete after a restore, or
    /// another app changed either record: the host keeps the user's confirmation with `hold`
    /// and calls again with it later. The service or store can fail too.
    pub async fn confirm_allowance_executor(
        &mut self,
        hold: &crate::AllowanceHold,
        _confirmed: UserConfirmed,
    ) -> Result<()> {
        Box::pin(self.apply_executor_grant(&hold.record(), Some(hold))).await
    }

    async fn apply_executor_grant(
        &mut self,
        record: &[u8; 32],
        confirmed: Option<&crate::AllowanceHold>,
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
        // An Allowance already known to have stopped for good refuses so first, kept grant or
        // not: nothing sent again changes it.
        let shown = self.allowance(record).await?;
        let consent = Box::pin(self.consent_state(record)).await?;
        if let Standing::Final(refusal) = standing(self, &shown, consent) {
            return Err(refusal.into());
        }
        // A first grant whose answer was lost lands next: until its batch is sent, nothing
        // more is decided, and once it applied, its consent is this call's to write.
        Box::pin(self.land_kept_grant(record)).await?;
        // Where the Allowance stands decides then, by the classification a payment under it
        // follows: the user is asked to confirm only where confirming lifts the refusal.
        let shown = self.allowance(record).await?;
        let consent = Box::pin(self.consent_state(record)).await?;
        let standing = standing(self, &shown, consent);
        match standing {
            Standing::Final(refusal) => return Err(refusal.into()),
            Standing::Temporary(why) => return Err(Refusal::NotNow(why.into()).into()),
            Standing::Confirm | Standing::Payable | Standing::NotTakenUp => {}
        }
        // The user's confirmation counts only under the hold it was given for: a restore or a
        // `missed` item since, the read just made included, sets a hold the user hasn't seen
        // (Paykit spec 7.2, 8.3). Checked after the last read, before anything is written.
        if confirmed.is_some_and(|given| *given != shown.hold)
            || (standing == Standing::Confirm && confirmed.is_none())
        {
            return Err(Refusal::AllowanceConfirmationNeeded { hold: shown.hold }.into());
        }
        if shown.granted_before_loss {
            return Box::pin(self.regrant_after_loss(record, &shown)).await;
        }
        if shown.regranted_after_loss {
            // Asked again under a newer hold after the regrant: the confirmation consents
            // under it, and the usage still waits for its repair.
            return self.consent_under_current_hold(record, &shown).await;
        }
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
            Box::pin(self.still_standing(record)).await?;
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
        Box::pin(self.still_standing(record)).await?;
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
        // Kept to send again, the batch leaves its ID at `gb/`, which it deletes when it
        // applies: a resume lands it first and never builds a second grant over it.
        // The user's approval goes with the batch, keyed by the hold it was given under: a
        // batch refused leaves none, and one kept lands it with the grant.
        let marker = key::grant_batch(record).into_bytes();
        let mut batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .entry(op)
            .local(state::local_put(&key, &ExecutorHistory(vec![checkpoint]))?)
            .keep_id_at(marker.clone())
            .local(LocalWrite::Delete { key: marker });
        if confirmed.is_some() {
            let hold = self.current_hold().await?;
            batch = batch.local(state::local_put(
                &key::execution_approval(record, hold.as_ref()),
                &true,
            )?);
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

    /// After a provider loss, the user's confirmation again (under the hold it was shown,
    /// checked by the caller) writes this app's grant once more, at the current generation
    /// (Paykit spec 8.3, "After provider loss"). The write is conditioned on the exact
    /// Allowance record read here, so an end or another change in between refuses it. It
    /// establishes no complete usage authority: automatic payment under the Allowance waits
    /// for its usage to be repaired, and the shared hold on one-time payments stays.
    async fn regrant_after_loss(
        &mut self,
        record: &[u8; 32],
        view: &crate::AllowanceView,
    ) -> Result<()> {
        let allow = allowance_target(&view.counterparty, &view.allowance_id);
        let current = self
            .app
            .read_record_application(&allow)
            .await?
            .ok_or_else(regrant_waits)?;
        // The read brings the service time up to date: an Allowance that expired since is
        // refused as ended before anything is written.
        Box::pin(self.still_standing(record)).await?;
        // A newer loss or hold learned during the read leaves the confirmation stale.
        let hold = self.allowance_hold(record).await?;
        if hold != view.hold {
            return Err(Refusal::AllowanceConfirmationNeeded { hold }.into());
        }
        let read = &current.record;
        let pubky_switch_client::app::RecordContent::Value(bytes) = &read.content else {
            return Err(regrant_waits().into());
        };
        if !read.authenticated
            || read.doubtful
            || read.flagged
            || read.not_taken
            || read.different_write_at_held_version
            || read.key != *record
            || self.pause_at(record).await?.adds_no_stop_waits()
        {
            return Err(regrant_waits().into());
        }
        let own = self.identity();
        let lost = self.app.own_identity().body().recovery_from;
        let mut next = AllowanceRecord::bind(bytes, &own, self.app.naming_key(), record)?;
        let paused = next.current_grant(&own).is_some_and(|(grant, _)| {
            grant.app_id == self.app_id()
                && lost.is_some_and(|lost| grant.confirmed_generation <= lost)
        });
        if !paused || next.end.is_some() || next.conflicted.is_some() {
            return Err(regrant_waits().into());
        }
        let terms_hash = next.terms_hash().ok_or_else(regrant_waits)?;
        let generation = self.app.own_identity().generation();
        // The write builds on the version the service holds the record's current write at:
        // after a recovery upload that can be lower than the version the write was first
        // given (Pubky Switch wire 6.3, 7.6).
        let base = current.installed_version;
        let fields = grant_fields(
            &own,
            &view.counterparty,
            &view.allowance_id,
            &terms_hash,
            generation,
            base,
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
            built_on: base,
            grant: grant.clone(),
            origin: Origin::default(),
        });
        next.grant = Some(grant);
        // Built on the one read checked above, with no read again in between: the write is
        // conditioned on that exact write and on the version the service installed it at.
        let (conditions, mut kind) = self
            .app
            .record_op(
                &allow,
                Some(&next.to_json()?),
                vec![Condition::RecordCurrent {
                    key: *record,
                    revision: current.application,
                    deleted: false,
                }],
            )
            .await?;
        let OpKind::RecordPut(put) = &mut kind else {
            return Err(regrant_waits().into());
        };
        put.expected_version = base;
        let op = (conditions, kind);
        let hold = self.current_hold().await?;
        // Kept to send again after a lost answer, the batch leaves its ID at `gb/`, which it
        // deletes when it applies: a call again lands it first and builds no second grant.
        let marker = key::grant_batch(record).into_bytes();
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .entry(op)
            .local(state::local_put(
                &key::execution_approval(record, hold.as_ref()),
                &true,
            )?)
            .keep_id_at(marker.clone())
            .local(LocalWrite::Delete { key: marker });
        self.app.submit_batch(batch).await?;
        // Applied now, under the hold the user confirmed: a `confirm again` mark from that
        // same hold is answered by this confirmation.
        if self.allowance_hold(record).await? == view.hold {
            if let Some(entry) = self
                .local_get::<ConsentEntry>(&key::consent(record))
                .await?
                .filter(|entry| {
                    matches!(
                        entry.consent,
                        Consent::Held {
                            confirm_again: true
                        }
                    )
                })
            {
                let mut tx = Tx::default();
                tx.put(
                    &key::consent(record),
                    &entry.with(Consent::Held {
                        confirm_again: false,
                    }),
                )?;
                self.local_commit(tx).await?;
            }
        }
        Ok(())
    }

    /// The user's confirmation, under the hold in force now, of an Allowance whose grant was
    /// written again after a provider loss: its approval and consent, in one local commit.
    async fn consent_under_current_hold(
        &mut self,
        record: &[u8; 32],
        view: &crate::AllowanceView,
    ) -> Result<()> {
        // Read fresh first: an end or another change since the last receive, a hold set in
        // between, or a stopped app, refuses as it would before a write.
        if let Some(alarm) = self.app.alarm() {
            return Err(pubky_switch_client::app::AppError::Alarm(alarm.clone()).into());
        }
        let allow = allowance_target(&view.counterparty, &view.allowance_id);
        self.app
            .read_record_application(&allow)
            .await?
            .ok_or_else(regrant_waits)?;
        Box::pin(self.still_standing(record)).await?;
        let shown = self.allowance(record).await?;
        if shown.hold != view.hold {
            return Err(Refusal::AllowanceConfirmationNeeded { hold: shown.hold }.into());
        }
        if !shown.regranted_after_loss {
            return Err(regrant_waits().into());
        }
        let hold = self.current_hold().await?;
        let mut tx = Tx::default();
        tx.put(&key::execution_approval(record, hold.as_ref()), &true)?;
        tx.put(
            &key::consent(record),
            &ConsentEntry::new(
                view.counterparty,
                &view.allowance_id,
                Consent::Held {
                    confirm_again: false,
                },
            ),
        )?;
        self.local_commit(tx).await
    }

    /// Refused for good or for now as [`standing`] says, read again after the fresh reads of
    /// the records, which bring the service time up to date: an Allowance that expired
    /// since the last receive refuses as ended before anything is written.
    async fn still_standing(&self, record: &[u8; 32]) -> Result<()> {
        let shown = self.allowance(record).await?;
        let consent = Box::pin(self.consent_state(record)).await?;
        match standing(self, &shown, consent) {
            Standing::Final(refusal) => Err(refusal.into()),
            Standing::Temporary(why) => Err(Refusal::NotNow(why.into()).into()),
            Standing::Confirm | Standing::Payable | Standing::NotTakenUp => Ok(()),
        }
    }

    /// Land this app's first grant of the Allowance at `record` when the client keeps its
    /// batch from a call whose answer was lost: a read sends kept batches again before the
    /// feed, so a resume or a confirmation retried without the host reading in between
    /// still converges. While the batch stays kept, nothing more is decided now; a batch the
    /// client applied or dropped leaves nothing to land.
    async fn land_kept_grant(&mut self, record: &[u8; 32]) -> Result<()> {
        let marker = key::grant_batch(record);
        let Some(id) = self.local_get::<[u8; 16]>(&marker).await? else {
            return Ok(());
        };
        let kept =
            |status: Option<KeptBatchStatus>| matches!(status, Some(KeptBatchStatus::Kept { .. }));
        if kept(self.app.kept_batch(id).await?) {
            // What the read brings is the host's to see at its next receive.
            let summary = Box::pin(self.receive(None)).await?;
            self.remember_read(summary);
            if kept(self.app.kept_batch(id).await?) {
                return Err(Refusal::NotNow(
                    "the executor grant's write waits to be sent again".into(),
                )
                .into());
            }
        }
        self.local_delete(&marker).await
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
