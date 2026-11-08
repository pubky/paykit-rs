//! Reading and writing Paykit's shared records through the client (Paykit spec 6, Paykit
//! wire 5): every record is bound with the reader's own naming key before it is acted on,
//! and every write is the client's compare-and-swap.
//!
//! A message whose sending changes a record is signed first, then the record's next value
//! is built from the signed message, holding its stored copy, and the record write and the
//! message go in one batch ([`Paykit::send_with_record`]): the message is queued only if
//! the write is applied, and the write is never made without the message (Switch spec 8.2).

use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::kinds::Kind;
use paykit_lib::records::{ActivityRecord, PaymentListRecord, StoredMessage};
use pubky_switch_client::app::{LocalWrite, OpBatch, OutgoingMessage, RecordTarget, SignedMessage};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::ops::{Condition, OpKind};
use serde_json::{Map, Value};

use crate::activity::{self, ActivityTarget, Role};
use crate::error::{PaykitError, Result};
use crate::paykit::Paykit;

impl Paykit {
    /// The record key of an activity target.
    pub(crate) fn activity_key(&self, target: &ActivityTarget) -> Result<[u8; 32]> {
        Ok(self.app.record_key(&target.record_target())?)
    }

    /// The activity record the app holds at `target`, bound to its key (Paykit wire 5.3);
    /// `None` when it holds none, or one that doesn't bind.
    pub(crate) async fn held_activity(
        &self,
        target: &ActivityTarget,
    ) -> Result<Option<ActivityRecord>> {
        let key = self.activity_key(target)?;
        let Some(held) = self.app.shared_record(&key).await? else {
            return Ok(None);
        };
        let Some(value) = held.content.value() else {
            return Ok(None);
        };
        Ok(ActivityRecord::bind(
            value,
            &self.identity(),
            target.side.side(),
            self.naming(),
            &key,
            target.label().as_ref(),
        )
        .ok())
    }

    /// The activity record at `target`, read from the switch when the app holds none.
    pub(crate) async fn activity(
        &mut self,
        target: &ActivityTarget,
    ) -> Result<Option<ActivityRecord>> {
        if let Some(record) = self.held_activity(target).await? {
            return Ok(Some(record));
        }
        self.app.read_record(&target.record_target()).await?;
        self.held_activity(target).await
    }

    /// The write of the record at `target` with `stored` placed in its `role` (Paykit spec 6
    /// "One message per role"), built on the version the app holds, with `conditions`; a
    /// record that doesn't read is written again with the message (Paykit wire 5.3).
    /// `None` when the record already reflects the message.
    pub(crate) async fn placed_write(
        &mut self,
        target: &ActivityTarget,
        counterparty: [u8; 32],
        role: Role,
        stored: StoredMessage,
        conditions: Vec<Condition>,
    ) -> Result<Option<(Vec<Condition>, OpKind)>> {
        let own = self.identity();
        let side = target.side.side();
        let key = self.activity_key(target)?;
        let held = self.app.shared_record(&key).await?;
        let current = held.as_ref().and_then(|r| r.content.value());
        let mut record = activity::read(current, &own, side)
            .unwrap_or_else(|| activity::empty(side, counterparty));
        if !activity::place(&mut record, role, stored) {
            return Ok(None);
        }
        let value = record.to_json()?;
        Ok(Some(
            self.app
                .record_op(&target.record_target(), Some(&value), conditions)
                .await?,
        ))
    }

    /// Every `payment_lists` record of `payee` the app holds that binds.
    pub(crate) async fn lists_of(&self, payee: &[u8; 32]) -> Result<Vec<PaymentListRecord>> {
        let own = self.identity();
        let mut out = Vec::new();
        for record in self.app.records_of(Collection::PaymentLists.name()).await? {
            let Some(value) = record.content.value() else {
                continue;
            };
            if let Ok(list) = PaymentListRecord::bind(value, &own, self.naming(), &record.key) {
                if &list.payee == payee {
                    out.push(list);
                }
            }
        }
        Ok(out)
    }

    /// Sign `body` once as a message of `kind` to `to`, keeping nothing and sending nothing
    /// (Switch spec 8.2): its hash is known before anything that names it is written.
    pub(crate) fn sign(
        &self,
        to: [u8; 32],
        kind: Kind,
        refs: Vec<[u8; 32]>,
        body: Map<String, Value>,
    ) -> Result<SignedMessage> {
        let mut message = OutgoingMessage::new(to, kind.as_str(), body);
        message.refs = refs;
        Ok(self.app.sign_message(message)?)
    }

    /// The stored copy of a message this app signed, as its own records hold it: its
    /// acceptance time isn't known yet (Paykit wire 5.2).
    pub(crate) fn own_copy(&self, signed: &SignedMessage) -> Result<StoredMessage> {
        Ok(StoredMessage::new(
            self.identity(),
            self.app.cert().signed(),
            signed.plaintext().clone(),
            None,
            &signed.to(),
        )?)
    }

    /// Place `signed` in its `role` of the record at `target` and queue it, in one batch with
    /// `conditions` on the record write and the local writes `local` (Switch spec 8.2): the
    /// message goes out only if the record write is applied. Returns the `msg_id`. Nothing is
    /// sent: the caller flushes.
    ///
    /// # Errors
    ///
    /// The client's errors. After `409 conflict` the record is read again first, so the
    /// caller decides again on what the switch holds; nothing was queued.
    pub(crate) async fn send_with_record(
        &mut self,
        signed: SignedMessage,
        target: &ActivityTarget,
        role: Role,
        conditions: Vec<Condition>,
        local: Vec<LocalWrite>,
    ) -> Result<String> {
        let msg_id = signed.msg_id().to_owned();
        let stored = self.own_copy(&signed)?;
        let write = self
            .placed_write(target, signed.to(), role, stored, conditions)
            .await?;
        let mut batch = OpBatch::new().queue(signed.clone()).locals(local.clone());
        match write {
            Some(entry) => batch = batch.entry(entry),
            // The record reflects it already: the message is queued alone, with the local
            // writes.
            None => {
                self.app.queue_signed(signed).await?;
                if !local.is_empty() {
                    self.app.protocol_commit(&local).await?;
                }
                return Ok(msg_id);
            }
        }
        match self.app.submit_batch(batch).await {
            Ok(_) => Ok(msg_id),
            Err(error) => {
                if error.code() == Some(ErrorCode::Conflict) {
                    self.app.read_record(&target.record_target()).await?;
                }
                Err(error.into())
            }
        }
    }
}

/// The client's target of the `payment_lists` record of a payee app.
pub(crate) fn list_target(payee: &[u8; 32], app_id: &[u8; 16]) -> RecordTarget {
    RecordTarget::new(
        activity::switch_collection(Collection::PaymentLists, None),
        &natural_key::payment_list(payee, app_id),
    )
}

/// A `409 conflict`: the caller reads again and decides again.
pub(crate) fn conflict(error: PaykitError) -> PaykitError {
    if error.code() == Some(ErrorCode::Conflict) {
        PaykitError::Conflict("the record changed meanwhile".into())
    } else {
        error
    }
}
