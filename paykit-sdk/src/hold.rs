//! The hold after a restore (Paykit spec 7.2, "What a restore loses"), one-time branch.
//!
//! A backup can predate a payee's cancellation of a request it holds open: the restored app
//! would see the request payable. So every restore from a backup holds every open request,
//! as the restore itself sets it: [`crate::protocol::PaykitProtocol`]'s restore step adds the
//! hold to the writes it installs, in the restore's one commit, so no restore can leave it
//! out; it is no part of what a backup carries, and a confirmation an older backup holds is
//! dropped by the restore. While a request is held:
//!
//! - no automatic attempt is made on it (part 1 makes none);
//! - a manual one only once the user has seen that the payee may have cancelled it
//!   ([`crate::PayOptions::possibly_cancelled_confirmed`]), which ends the hold for that
//!   request;
//! - a cancellation the record shows, or the app keeps, stops it for good, and no
//!   confirmation overrides it (Paykit spec 8.1).
//!
//! After the restore, at the reconciliation it requires, once the records are read again,
//! the app asks every counterparty with an open request to re-send what its inbox accepted
//! from the acceptance time of the last entry the backup's cursor had passed, less the
//! clock tolerance (Paykit spec 9). A cancellation that comes back is kept
//! and placed as any other.

use paykit_lib::records::ActivityRecord;
use pubky_switch_core::records::CLOCK_TOLERANCE_MS;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::paykit::Paykit;
use crate::protocol::QueuedAsk;
use crate::requests::{cancelled, rejected, RequestStatus};
use crate::state::{key, Tx};

/// The hold a restore set (`p/rh`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RestoreHold {
    /// From when the counterparties are asked to re-send: the acceptance time of the last
    /// entry the backup's cursor passed, less the clock tolerance.
    pub(crate) resend_from_ms: u64,
    /// The re-sends are queued.
    pub(crate) asks_queued: bool,
}

impl RestoreHold {
    /// The hold of a restore whose backup's cursor had passed entries accepted up to
    /// `cursor_time_ms`.
    pub(crate) fn of(cursor_time_ms: u64) -> Self {
        Self {
            resend_from_ms: cursor_time_ms.saturating_sub(CLOCK_TOLERANCE_MS),
            asks_queued: false,
        }
    }
}

impl Paykit {
    /// Whether the request record at `record_key`, `record`, is held after a restore: a
    /// restore set the hold, the user hasn't confirmed this request since, and it is open.
    /// Every open request is held, one first seen after the restore too: no time the app
    /// can see proves a request new, since its creation time is the counterparty's and a
    /// request from before the backup can come again with a later acceptance time (Paykit
    /// spec 7.2, "What a restore loses": nothing releases the hold automatically).
    pub(crate) async fn held(
        &self,
        record_key: &[u8; 32],
        record: &ActivityRecord,
    ) -> Result<bool> {
        if self
            .local_get::<RestoreHold>(key::RESTORE_HOLD)
            .await?
            .is_none()
        {
            return Ok(false);
        }
        if record.shows_paid() || cancelled(record) || rejected(record) {
            return Ok(false);
        }
        Ok(self
            .local_get::<bool>(&key::hold_confirmed(record_key))
            .await?
            .is_none())
    }

    /// Queue, once after a restore, a `resend_request` to every counterparty with an open
    /// request, from the restore's re-send time; [`Paykit::receive`] sends them.
    pub(crate) async fn queue_restore_asks(&mut self) -> Result<()> {
        let Some(mut hold) = self.local_get::<RestoreHold>(key::RESTORE_HOLD).await? else {
            return Ok(());
        };
        if hold.asks_queued {
            return Ok(());
        }
        let mut counterparties = std::collections::BTreeSet::new();
        for view in self.requests().await? {
            let open = !matches!(
                view.status,
                RequestStatus::Paid
                    | RequestStatus::Cancelled
                    | RequestStatus::Rejected
                    | RequestStatus::Expired
            );
            if open {
                counterparties.insert(view.request.payee);
            }
        }
        let mut tx = Tx::default();
        for counterparty in counterparties {
            tx.put(
                &key::ask(&counterparty, hold.resend_from_ms),
                &QueuedAsk {
                    sender: counterparty,
                    from_ms: hold.resend_from_ms,
                    until_ms: None,
                },
            )?;
        }
        hold.asks_queued = true;
        tx.put(key::RESTORE_HOLD, &hold)?;
        self.local_commit(tx).await
    }
}
