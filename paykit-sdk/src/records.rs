//! Reading and writing Paykit's shared records through the client (Paykit spec 6, Paykit
//! wire 5): every record is bound with the reader's own naming key before it is acted on,
//! and every write is the client's compare-and-swap.

use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::kinds::Kind;
use paykit_lib::records::{ActivityRecord, PaymentListRecord, StoredMessage};
use pubky_switch_client::app::{OutgoingMessage, RecordJob, RecordTarget};
use pubky_switch_core::ops::Condition;
use serde_json::{Map, Value};

use crate::activity::{self, ActivityTarget, JobTag, Role};
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
        Box::pin(self.app.read_record(&target.record_target())).await?;
        self.held_activity(target).await
    }

    /// Place `stored` in its `role` of the record at `target`, compare-and-swap, building it
    /// again on what the switch holds after a conflict. A record that doesn't read is
    /// written again with the message (the reader doesn't act on an invalid record, Paykit
    /// wire 5.3). Returns whether the record changed.
    pub(crate) async fn place(
        &mut self,
        target: &ActivityTarget,
        counterparty: [u8; 32],
        role: Role,
        stored: StoredMessage,
        conditions: Vec<Condition>,
    ) -> Result<bool> {
        let own = self.identity();
        let side = target.side.side();
        let written = Box::pin(self.app.update_record(
            &target.record_target(),
            conditions,
            move |current| {
                let mut record = activity::read(current, &own, side)
                    .unwrap_or_else(|| activity::empty(side, counterparty));
                if !activity::place(&mut record, role, stored.clone()) {
                    return None;
                }
                record.to_json().ok()
            },
        ))
        .await
        .map_err(conflict)?;
        Ok(written.is_some())
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

    /// Sign `body` once as a message of `kind` to `to` and keep it in the outbox, with the
    /// record job that stores it in its `role` of the record at `record` (Switch spec 8.2,
    /// 11). Nothing is sent: the next flush sends it.
    pub(crate) async fn queue_message(
        &mut self,
        to: [u8; 32],
        kind: Kind,
        refs: Vec<[u8; 32]>,
        body: Map<String, Value>,
        record: Option<(&ActivityTarget, Role)>,
    ) -> Result<String> {
        let record = match record {
            Some((target, role)) => Some(RecordJob {
                target: target.record_target(),
                tag: serde_json::to_vec(&JobTag {
                    side: target.side,
                    role,
                })
                .map_err(|_| PaykitError::corrupt())?,
            }),
            None => None,
        };
        let message = OutgoingMessage {
            to,
            kind: kind.as_str().to_owned(),
            refs,
            body,
            notify: true,
            invite_code: None,
            record,
        };
        Ok(Box::pin(self.app.queue(message)).await?)
    }
}

/// The client's target of the `payment_lists` record of a payee app.
pub(crate) fn list_target(payee: &[u8; 32], app_id: &[u8; 16]) -> RecordTarget {
    RecordTarget::new(
        activity::switch_collection(Collection::PaymentLists, None),
        &natural_key::payment_list(payee, app_id),
    )
}

/// A `409 conflict` after the client's retries: the caller reads again and decides again.
pub(crate) fn conflict(error: pubky_switch_client::app::AppError) -> PaykitError {
    if error.code() == Some(pubky_switch_core::http::ErrorCode::Conflict) {
        PaykitError::Conflict("the record changed meanwhile".into())
    } else {
        PaykitError::Client(error)
    }
}
