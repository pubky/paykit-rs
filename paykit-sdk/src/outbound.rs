//! Outgoing writes and messages wait for a restore's first read (Switch spec 12.4).

use pubky_switch_client::app::{AppError, Delivery, OpBatch, OutboxReport, Refusal};
use pubky_switch_client::client::Submitted;

use crate::error::Result;
use crate::paykit::Paykit;

impl Paykit {
    /// The complete queued inventory, under uninterrupted host namespace ownership and
    /// after prior operations have resolved. A partial owner set proves no source absent.
    pub(crate) async fn queued_message_ids(&self) -> Result<std::collections::BTreeSet<String>> {
        let protocols = self.app.protocols().names();
        if !protocols
            .iter()
            .any(|name| name == crate::protocol::BACKUP_NAME)
        {
            return Err(crate::error::Refusal::NotNow(
                "Switch spec 11: delivery ownership is not established".into(),
            )
            .into());
        }
        let mut queued = std::collections::BTreeSet::new();
        for protocol in protocols {
            for message in self.app.queued_messages(&protocol).await? {
                queued.insert(message.msg_id);
            }
        }
        Ok(queued)
    }

    /// A restore in reconcile mode has no verdict until its identity-confirmed first page.
    /// A directory may settle the verdict while adopting an identity not yet confirmed
    /// on the Homeserver. A load repeats that unsettled read; the verdict alone is not enough.
    pub(crate) fn first_read_pending(&self) -> bool {
        !self.first_read_done
            && self.app.reconcile_mode()
            && (self.app.restore_held_cursor().is_none()
                || self.app.identity_confirmed_at().is_none())
    }

    /// Allow this payee to start a conversation, including a pending connection request
    /// (Switch spec 7). A restored wallet completes its required first read first, and
    /// its next receive reports those effects (Switch spec 12.4).
    ///
    /// # Errors
    ///
    /// The first read's failure, or the Switch client's refusal without `conversation.add`.
    pub async fn allow_connect(&mut self, payee: &[u8; 32]) -> Result<()> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        Ok(self.app.allow_connect(payee).await?)
    }

    /// Confirm that the user recognizes these post-backup registrations as this device's
    /// own (Switch spec 12.4). Pass the numbers from the current restore question.
    ///
    /// # Errors
    ///
    /// The first read's failure, a changed or empty question, or a store failure.
    pub async fn confirm_restored_registrations(
        &mut self,
        registrations: &[u64],
        confirmed: crate::UserConfirmed,
    ) -> Result<()> {
        self.check_restore_question(registrations).await?;
        Ok(self.app.confirm_restored_registrations(confirmed).await?)
    }

    /// Report that the user cannot account for these post-backup registrations (Switch
    /// spec 12.4). The client raises its compromise alarm and requests revocation.
    ///
    /// # Errors
    ///
    /// The first read's failure, a changed or empty question, or the client's alarm.
    pub async fn refuse_restored_registrations(&mut self, registrations: &[u64]) -> Result<()> {
        self.check_restore_question(registrations).await?;
        Ok(self.app.refuse_restored_registrations().await?)
    }

    async fn check_restore_question(&mut self, registrations: &[u64]) -> Result<()> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        if registrations.is_empty() || self.app.restore_question() != registrations {
            return Err(crate::Refusal::NotNow("read the current restore question".into()).into());
        }
        Ok(())
    }

    /// Read before preparing a public operation's effects. The host receives the summary
    /// with its next receive; durable duty events remain the client's (Switch spec 12.4).
    pub(crate) async fn first_read_before_writing(&mut self) -> Result<()> {
        if self.first_read_pending() {
            // Allow one listing page per registered Paykit collection. A larger listing
            // keeps its continuation; an unavailable key, identity or feed still holds.
            let reads = crate::hold::RecordSide::All
                .collections()
                .len()
                .div_ceil(pubky_switch_client::app::LISTING_PAGES_PER_READ as usize);
            for _ in 0..reads {
                let summary = Box::pin(self.receive(None)).await?;
                let more = summary.report.more && !summary.report.stopped;
                self.first_read_summary = Some(Box::new(summary));
                if !self.first_read_pending() || !more || !self.app.listing_owed().await? {
                    break;
                }
            }
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
