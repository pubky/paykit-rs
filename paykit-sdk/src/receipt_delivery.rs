//! An imported receipt batch can establish original delivery intent (Paykit spec 7.2).

use paykit_lib::collections::Collection;
use paykit_lib::records::{ActivitySide, StoredMessage};
use pubky_switch_client::app::{KeptOp, RecordRead, RecordTarget, SharedRecord};
use pubky_switch_core::ident::collection_id;
use pubky_switch_core::ops::{Condition, OpKind, SignedOp};
use pubky_switch_core::records::{AppCert, IdentityDoc};
use pubky_switch_core::sealed::RecordValue;
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};

use crate::activity::ActivityTarget;
use crate::error::{PaykitError, Result};
use crate::paykit::Paykit;
use crate::receipts::{displayed_receipt, receipt_binding, ReceiptBinding};
use crate::state::StateKey;

/// The client first writes the sixteen-byte ID. A verified answer for this exact write
/// can replace the ID while delivery itself is still uncertain.
#[derive(Deserialize)]
pub(crate) enum ReceiptBatch {
    Awaiting([u8; 16]),
    Applied(Box<ReceiptApplication>),
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ReceiptApplication {
    pub(crate) batch: [u8; 16],
    identity: SignedDoc,
    write: KeptOp,
}

impl ReceiptBatch {
    pub(crate) fn id(&self) -> [u8; 16] {
        match self {
            Self::Awaiting(id) => *id,
            Self::Applied(applied) => applied.batch,
        }
    }
}

impl ReceiptApplication {
    /// An earlier identity document establishes the service authority of this write alone.
    pub(crate) fn original(
        &self,
        identity: &[u8; 32],
        app_id: &[u8; 16],
        record: &[u8; 32],
    ) -> Option<SignedOp> {
        let doc = IdentityDoc::verify(&self.identity, identity).ok()?;
        let cert = AppCert::verify(&self.write.cert, identity).ok()?;
        let op = SignedOp::verify_with_cert(&self.write.op, &cert, identity).ok()?;
        let OpKind::RecordPut(put) = &op.body().kind else {
            return None;
        };
        let dated =
            pubky_switch_core::shared::date_write(self.write.proof.as_ref()?, &op, &doc).ok()?;
        if dated.version.is_none()
            || op.body().generation != doc.generation()
            || op.body().batch.id != self.batch
            || cert.app_id() != *app_id
            || !doc.standing(app_id, &cert.hash()).counts_at(dated.time)
            || cert.expired_at(dated.time)
            || put.key != *record
            || put.collection != collection_id(Collection::PayeeActivity.name(), identity)
            || put.value_sealed.scope != put.policy.read_scope
            || !put
                .policy
                .write_scopes
                .iter()
                .any(|scope| cert.holds_scope(scope))
            || paykit_lib::scopes::check_certificate(&cert).is_err()
        {
            return None;
        }
        Some(op)
    }
}

impl Paykit {
    /// Preserve a read-only, authenticated answer for an unanswered send before another
    /// app can replace this record. Reading the record never submits its kept batch.
    pub(crate) async fn remember_receipt_application(
        &mut self,
        target: &RecordTarget,
        marker: &str,
        batch: [u8; 16],
    ) -> Result<()> {
        self.retire_refused_receipt_batch(Some(marker)).await?;
        if !matches!(self.local_get::<ReceiptBatch>(marker).await?, Some(ReceiptBatch::Awaiting(id)) if id == batch)
        {
            return Ok(());
        }
        let RecordRead::Fresh(current) = self.app.read_record_fresh(target).await? else {
            return Ok(());
        };
        let Some((target, stored)) = self.current_receipt(&current).await? else {
            return Ok(());
        };
        if crate::state::key::receipt_batch(
            &current.key,
            &stored.hash(),
            &stored.message().msg_id,
            false,
        ) != marker
        {
            return Ok(());
        }
        let doc = self.app.own_identity().signed();
        for write in self.app.shared_evidence().await?.ops {
            if !self.current_receipt_batch(&write, &current, batch) {
                continue;
            }
            let applied = ReceiptApplication {
                batch,
                identity: doc.clone(),
                write,
            };
            if self
                .historical_receipt(&applied, &current, &target)
                .await?
                .is_some_and(|old| same_receipt(&old, &stored))
            {
                self.local_put(marker, &applied).await?;
                break;
            }
        }
        Ok(())
    }

    /// Bind the restored intent to its exact applied receipt and the current shared record.
    pub(crate) async fn note_imported_receipt_batches(&mut self) -> Result<()> {
        for (suffix, _) in self.local_scan::<ReceiptBatch>("ri/").await? {
            self.retire_refused_receipt_batch(Some(&format!("ri/{suffix}")))
                .await?;
        }
        let markers = self.local_scan::<ReceiptBatch>("ir/").await?;
        let mut evidence = None;
        for (suffix, state) in markers {
            let marker = format!("ir/{suffix}");
            let Some(StateKey::ReceiptBatch {
                record,
                hash,
                msg_id,
                ..
            }) = StateKey::parse(marker.as_bytes())
            else {
                return Err(PaykitError::corrupt());
            };
            let Some(current) = self.app.shared_record(&record).await? else {
                continue;
            };
            let Some((target, stored)) = self.current_receipt(&current).await? else {
                continue;
            };
            if stored.hash() != hash || stored.message().msg_id != msg_id {
                continue;
            }
            let matches = match state {
                ReceiptBatch::Applied(applied) => self
                    .historical_receipt(&applied, &current, &target)
                    .await?
                    .is_some_and(|old| same_receipt(&old, &stored)),
                ReceiptBatch::Awaiting(batch) => {
                    if evidence.is_none() {
                        evidence = Some(self.app.shared_evidence().await?);
                    }
                    current.writer == self.app_id()
                        && evidence.as_ref().is_some_and(|held| {
                            held.ops
                                .iter()
                                .any(|kept| self.current_receipt_batch(kept, &current, batch))
                        })
                }
            };
            if matches {
                self.note_restored_delivery(&stored).await?;
                self.local_delete(&marker).await?;
            }
        }
        Ok(())
    }

    fn current_receipt_batch(&self, kept: &KeptOp, record: &SharedRecord, batch: [u8; 16]) -> bool {
        let Ok(op) = SignedOp::verify_with_cert(&kept.op, self.app.cert(), &self.identity()) else {
            return false;
        };
        op.body().batch.id == batch
            && op.body().generation == record.version.generation
            && matches!(&op.body().kind, OpKind::RecordPut(put) if put.key == record.key)
    }

    async fn current_receipt(
        &self,
        held: &SharedRecord,
    ) -> Result<Option<(ActivityTarget, StoredMessage)>> {
        if held.collection != Collection::PayeeActivity.name()
            || !held.authenticated
            || held.flagged
            || held.not_taken
            || held.doubtful
            || held.different_write_at_held_version
            || held.divergent
            || self.app.record_condition(&held.key).await?
                != (Condition::RecordVersion {
                    key: held.key,
                    version: held.version,
                })
        {
            return Ok(None);
        }
        let Ok((target, activity)) = self.bound_payee_record(held) else {
            return Ok(None);
        };
        let parent = self.payee_request_record(&target, &activity).await?;
        if receipt_binding(&activity, parent.as_ref()) != ReceiptBinding::Verified {
            return Ok(None);
        }
        Ok(displayed_receipt(&activity)
            .filter(|stored| {
                stored.sender() == self.identity() && stored.cert().app_id() == self.app_id()
            })
            .map(|stored| (target, stored.clone())))
    }

    async fn historical_receipt(
        &self,
        applied: &ReceiptApplication,
        current: &SharedRecord,
        target: &ActivityTarget,
    ) -> Result<Option<StoredMessage>> {
        let Some(op) = applied.original(&self.identity(), &self.app_id(), &current.key) else {
            return Ok(None);
        };
        let OpKind::RecordPut(put) = &op.body().kind else {
            return Ok(None);
        };
        if put.policy != current.policy {
            return Ok(None);
        }
        let sealed = &put.value_sealed;
        let Some(key) = self.app.scope_key(sealed.data_epoch, &sealed.scope) else {
            return Ok(None);
        };
        let Ok(RecordValue::Json(value)) = sealed.open(&key, &self.identity(), &put.key) else {
            return Ok(None);
        };
        let Some(activity) = crate::records::bind_activity(
            &value,
            &self.identity(),
            ActivitySide::Payee,
            target.label().as_ref(),
            &put.key,
            |t| self.app.record_key(t).ok(),
            &|p| self.rail.settlement_id(p),
        ) else {
            return Ok(None);
        };
        let parent = self.payee_request_record(target, &activity).await?;
        Ok(
            (receipt_binding(&activity, parent.as_ref()) == ReceiptBinding::Verified)
                .then(|| displayed_receipt(&activity).cloned())
                .flatten(),
        )
    }
}

fn same_receipt(first: &StoredMessage, later: &StoredMessage) -> bool {
    first.hash() == later.hash()
        && first.message().msg_id == later.message().msg_id
        && first.sender() == later.sender()
        && first.cert().app_id() == later.cert().app_id()
}
