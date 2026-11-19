//! An imported receipt batch can establish original delivery intent (Paykit spec 7.2).

use std::collections::BTreeSet;

use paykit_lib::collections::Collection;
use paykit_lib::records::StoredMessage;
use pubky_switch_core::ops::{Condition, OpKind, SignedOp};

use crate::error::{PaykitError, Result};
use crate::paykit::Paykit;
use crate::receipts::{displayed_receipt, receipt_binding, ReceiptBinding};
use crate::state::{key, StateKey};

impl Paykit {
    /// Match the first keep in the selected backup to the same app's signed, applied
    /// receipt write. Only the original outbox could say whether delivery finished.
    pub(crate) async fn note_imported_receipt_batches(&mut self) -> Result<()> {
        // A crash can interrupt cleanup of a definitive refusal before the caller returns.
        for (suffix, _) in self.local_scan::<[u8; 16]>("ri/").await? {
            self.retire_refused_receipt_batch(Some(&format!("ri/{suffix}")))
                .await?;
        }
        let markers = self.local_scan::<[u8; 16]>("ir/").await?;
        if markers.is_empty() {
            return Ok(());
        }
        let mut records = BTreeSet::new();
        for (suffix, batch) in markers {
            let marker = format!("ir/{suffix}");
            let Some(StateKey::ReceiptBatch { record, .. }) = StateKey::parse(marker.as_bytes())
            else {
                return Err(PaykitError::corrupt());
            };
            records.insert((record, batch));
        }
        for kept in self.app.shared_evidence().await?.ops {
            let Ok(op) = SignedOp::peek(&kept.op) else {
                continue;
            };
            let OpKind::RecordPut(put) = &op.body().kind else {
                continue;
            };
            let batch = op.body().batch.id;
            if !records.contains(&(put.key, batch))
                || SignedOp::verify_with_cert(&kept.op, self.app.cert(), &self.identity()).is_err()
            {
                continue;
            }
            let Some(stored) = self
                .original_applied_receipt(&put.key, op.body().generation)
                .await?
            else {
                continue;
            };
            let marker =
                key::receipt_batch(&put.key, &stored.hash(), &stored.message().msg_id, true);
            if self.local_get::<[u8; 16]>(&marker).await? == Some(batch) {
                self.note_restored_delivery(&stored).await?;
                self.local_delete(&marker).await?;
            }
        }
        Ok(())
    }

    async fn original_applied_receipt(
        &self,
        record: &[u8; 32],
        generation: u64,
    ) -> Result<Option<StoredMessage>> {
        let Some(held) = self.app.shared_record(record).await? else {
            return Ok(None);
        };
        if held.collection != Collection::PayeeActivity.name()
            || !held.authenticated
            || held.flagged
            || held.not_taken
            || held.doubtful
            || held.different_write_at_held_version
            || held.divergent
            || held.version.generation != generation
            || held.writer != self.app_id()
            || self.app.record_condition(record).await?
                != (Condition::RecordVersion {
                    key: *record,
                    version: held.version,
                })
        {
            return Ok(None);
        }
        let Ok((target, activity)) = self.bound_payee_record(&held) else {
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
            .cloned())
    }
}
