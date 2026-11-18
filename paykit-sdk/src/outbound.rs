//! Outgoing writes and messages wait for a restore's first read (Switch spec 12.4).

use pubky_switch_client::app::{AppError, Delivery, OpBatch, OutboxReport, Refusal};
use pubky_switch_client::client::Submitted;

use crate::error::Result;
use crate::paykit::Paykit;

impl Paykit {
    /// A restore in reconcile mode has no verdict until its identity-confirmed first page.
    /// A directory may settle the verdict while adopting an identity not yet confirmed
    /// on the Homeserver. A load repeats that unsettled read; the verdict alone is not enough.
    pub(crate) fn first_read_pending(&self) -> bool {
        !self.first_read_done
            && self.app.reconcile_mode()
            && (self.app.restore_held_cursor().is_none()
                || self.app.identity_confirmed_at().is_none())
    }

    /// Read before preparing a public operation's effects. The host receives the summary
    /// with its next receive; durable duty events remain the client's (Switch spec 12.4).
    pub(crate) async fn first_read_before_writing(&mut self) -> Result<()> {
        if self.first_read_pending() {
            let summary = Box::pin(self.receive(None)).await?;
            self.first_read_summary = Some(Box::new(summary));
        }
        self.require_first_read()?;
        // A host can read through the App, or reload between its commit and Paykit's.
        if self.certificate_refused().is_none() && Box::pin(self.take_restore_verdict()).await? {
            Box::pin(self.refresh_lists_when_due()).await?;
        }
        Ok(())
    }

    pub(crate) fn require_first_read(&self) -> std::result::Result<(), AppError> {
        if self.first_read_pending() {
            return Err(AppError::Refused(Refusal::NotNow(
                "Switch spec 12.4: outgoing work awaits the restore's first read".into(),
            )));
        }
        Ok(())
    }

    pub(crate) async fn send_message(
        &mut self,
        msg_id: &str,
    ) -> std::result::Result<Option<Delivery>, AppError> {
        self.require_first_read()?;
        Box::pin(self.app.flush_message(msg_id)).await
    }

    pub(crate) async fn send_outbox(&mut self) -> std::result::Result<OutboxReport, AppError> {
        self.require_first_read()?;
        Box::pin(self.app.flush_outbox()).await
    }

    pub(crate) async fn submit(
        &mut self,
        batch: OpBatch,
    ) -> std::result::Result<Submitted, AppError> {
        self.require_first_read()?;
        Box::pin(self.app.submit_batch(batch)).await
    }

    pub(crate) async fn send_again(
        &mut self,
        msg_id: &str,
    ) -> std::result::Result<Delivery, AppError> {
        self.require_first_read()?;
        Box::pin(self.app.resend(msg_id)).await
    }

    pub(crate) async fn deny_at_switch(
        &mut self,
        pubky: &[u8; 32],
    ) -> std::result::Result<(), AppError> {
        self.require_first_read()?;
        Box::pin(self.app.deny(pubky)).await
    }

    pub(crate) async fn undeny_at_switch(
        &mut self,
        pubky: &[u8; 32],
    ) -> std::result::Result<(), AppError> {
        self.require_first_read()?;
        Box::pin(self.app.undeny(pubky)).await
    }
}
