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
            return Err(crate::error::Refusal::Usage(
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
    /// The first read's failure, or the Switch client's refusal without `reply.add`.
    pub async fn allow_connect(&mut self, payee: &[u8; 32]) -> Result<()> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
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
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
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
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        self.check_restore_question(registrations).await?;
        Ok(self.app.refuse_restored_registrations().await?)
    }

    async fn check_restore_question(&mut self, registrations: &[u64]) -> Result<()> {
        // Compare the answer with the durable registrations even after a lost commit reply.
        self.app.pending_duty_events().await?;
        if let Some(alarm) = self.app.alarm() {
            return Err(AppError::Alarm(alarm.clone()).into());
        }
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        if registrations.is_empty() || self.app.restore_question() != registrations {
            return Err(crate::Refusal::Usage("read the current restore question".into()).into());
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
        let _delivery = self.delivery_operation();
        self.service_time_for_delivery().await?;
        Box::pin(self.app.flush_message(msg_id)).await
    }

    pub(crate) async fn send_outbox(&mut self) -> std::result::Result<OutboxReport, AppError> {
        self.require_first_read()?;
        // What a restore onto an empty store holds goes once its first read is complete, also
        // when the host read through the client itself.
        self.release_restore_holds().await?;
        let _delivery = self.delivery_operation();
        self.service_time_for_delivery().await?;
        Box::pin(self.app.flush_outbox()).await
    }

    /// After a restore onto an empty store, release once every message of Paykit's it holds,
    /// once its first read is complete ([`Paykit::require_first_read`]), with the reply grant
    /// each asks for (Switch spec 12.4, "The outbox"). Such a restore holds only what the
    /// backup's outbox carried, which Paykit delivered at that point before the client held
    /// it. After a restore in place, which also holds what that store queued after the backup,
    /// nothing is released: the decisions of Paykit spec 7.2, "What a restore loses", aren't
    /// made yet, and the client's `MessagesHeldAtRestore` duty stays owed. A message whose
    /// release fails stays held for the next read or flush, and the others are released; one
    /// past its replay deadline is the client's to give up. The duty is acknowledged for
    /// Paykit once none is held. A message the host holds itself is never released here.
    ///
    /// # Errors
    ///
    /// A store failure or an alarm, after every other message was tried.
    pub(crate) async fn release_restore_holds(&mut self) -> std::result::Result<(), AppError> {
        use pubky_switch_client::app::{AppEvent, ReplyGrantOnRelease};
        let protocol = crate::protocol::BACKUP_NAME;
        let to_corrupt = |error: crate::error::PaykitError| match error {
            crate::error::PaykitError::Client(error) => error,
            _ => AppError::Store(pubky_switch_client::store::StoreError::Corrupt),
        };
        if self
            .local_get::<bool>(crate::state::key::RESTORE_RELEASE)
            .await
            .map_err(to_corrupt)?
            .is_none()
        {
            return Ok(());
        }
        let mut failed = None;
        for msg_id in self.app.held_messages(protocol).await? {
            match self
                .app
                .release_message(protocol, &msg_id, ReplyGrantOnRelease::IfOwed)
                .await
            {
                Ok(()) => {}
                Err(error @ (AppError::Store(_) | AppError::Alarm(_))) => {
                    failed.get_or_insert(error);
                }
                Err(_) => {}
            }
        }
        if let Some(error) = failed {
            return Err(error);
        }
        if !self.app.held_messages(protocol).await?.is_empty() {
            return Ok(());
        }
        self.local_delete(crate::state::key::RESTORE_RELEASE)
            .await
            .map_err(to_corrupt)?;
        let told: Vec<_> = self
            .app
            .pending_duty_events()
            .await?
            .into_iter()
            .filter(|duty| {
                matches!(duty.event, AppEvent::MessagesHeldAtRestore { .. })
                    && duty.owners.iter().any(|owner| owner == protocol)
            })
            .filter_map(|duty| duty.event.id())
            .collect();
        for id in told {
            self.ack_taken(id).await.map_err(to_corrupt)?;
        }
        Ok(())
    }

    /// Before the client delivers, keep the highest verified service time as `sv` and make
    /// the delivery operation observe one when a held message waits for it (Paykit spec
    /// 8.1, [`crate::send_holds::DeliveryClock`]).
    async fn service_time_for_delivery(&mut self) -> std::result::Result<(), AppError> {
        let kept = async {
            Box::pin(self.note_service_time()).await?;
            Box::pin(self.observe_for_delivery()).await
        };
        kept.await.map_err(|error| match error {
            crate::error::PaykitError::Client(error) => error,
            _ => AppError::Store(pubky_switch_client::store::StoreError::Corrupt),
        })
    }

    pub(crate) async fn submit(
        &mut self,
        batch: OpBatch,
    ) -> std::result::Result<Submitted, AppError> {
        self.require_first_read()?;
        let submitted = Box::pin(self.app.submit_batch(batch)).await;
        if submitted.is_ok() {
            self.observe_service_time();
        }
        submitted
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
