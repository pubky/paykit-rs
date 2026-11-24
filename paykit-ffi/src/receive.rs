use crate::{Divergence, NativeWallet, PaymentRefusal, WalletError, WalletEventData};
use paykit_sdk::Paykit;
use pubky_switch_client::app::{AppEvent, EventId, HeldCursor, Woken};
use std::sync::Arc;

/// One event in a receive report, with its durable duty identity when applicable.
#[derive(uniffi::Record)]
pub struct WalletEvent {
    /// Opaque duty ID to use when acknowledging a processed prefix.
    pub id: Option<Vec<u8>>,
    /// The protocols that still own this duty.
    pub owners: Vec<String>,
    /// The typed outcome of the receive operation.
    pub data: WalletEventData,
}
fn event(app: &Paykit, e: AppEvent) -> WalletEvent {
    let id = e.id();
    WalletEvent {
        id: id.map(|id| id.to_bytes().to_vec()),
        owners: id
            .and_then(|id| app.app().event_owners(id))
            .unwrap_or_default()
            .to_vec(),
        data: e.into(),
    }
}

/// What the cursor the service holds says beside the app's own position (Switch spec 8.3).
#[derive(uniffi::Enum)]
pub enum CursorStatus {
    /// At or behind the app's position: no device of the app passed an entry after it, so every
    /// entry after it is still at the service, but for those a `missed` item shows deleted and
    /// those sealed to a receive key the app doesn't hold.
    Clean,
    /// Ahead of the app's position: an instance of the app passed entries after it that the app's
    /// state doesn't hold, and the service may have deleted them since.
    Ahead {
        /// The `seq` the service reports.
        held: u64,
        /// The `seq` of the app's position it was compared with.
        position: u64,
    },
    /// Nothing to tell: a position a provider loss ended, an app whose devices share its cursor
    /// reported ahead, or, at a restore, a condition the client checks that didn't hold.
    Unknown,
    /// A native case that this binding does not recognize.
    Other,
}
impl From<HeldCursor> for CursorStatus {
    fn from(value: HeldCursor) -> Self {
        match value {
            HeldCursor::Clean => Self::Clean,
            HeldCursor::Ahead { held, position, .. } => Self::Ahead { held, position },
            HeldCursor::Unknown => Self::Unknown,
            _ => Self::Other,
        }
    }
}
/// What a `Paykit::receive` did.
#[derive(uniffi::Record)]
pub struct WalletReceiveReport {
    /// The provider generation of the receive cursor.
    pub generation: u64,
    /// The verified sequence reached in that generation.
    pub sequence: u64,
    /// Whether processing stopped at an input that still needs attention.
    pub stopped: bool,
    /// Whether more feed or inventory work remains.
    pub more: bool,
    /// The authenticated status of the restored cursor, when being reconciled.
    pub held_cursor: Option<CursorStatus>,
    /// Switch events in their reported order.
    pub events: Vec<WalletEvent>,
    /// The `msg_id`s of the messages received, each with its effect committed with the cursor
    /// move (Switch spec 8.2).
    pub applied: Vec<String>,
    /// The `resend_request`s answered and asked.
    pub resends: u64,
    /// A recovery ask waits because its sender certificate lacks `paykit_contacts`.
    pub resend_refused: Option<Arc<PaymentRefusal>>,
    /// The app's certificate fails Paykit's checks, as `Paykit::certificate_refused` gives it:
    /// the read went on, holding Paykit's entries unprocessed, and none of the Paykit steps after
    /// it ran.
    pub certificate_refused: Option<Arc<PaymentRefusal>>,
    /// The wallet's required listing under a new certificate or after a missed feed is
    /// incomplete.
    pub records_unlisted: Option<String>,
    /// A certificate taken is another than the one the records were last listed under, and the
    /// app waits for its scope keys, which the signer stores when it is next online (Switch spec
    /// 3.5).
    pub scope_keys_awaited: bool,
    /// The data epochs whose scope keys entries stored by the client wait for, ascending, as the
    /// read's `AppEvent::AwaitingKeys` names them: such an entry's record holds a value sealed at
    /// that epoch, which the app can't open yet, or its write needs the current epoch's key.
    pub key_epochs_awaited: Vec<u64>,
    /// The account is gone at this switch: the client took it as erased (Switch spec 12.7), a
    /// provider loss (Switch spec 12.6).
    pub account_gone: bool,
    /// The records this receive paused: a view showed them divergent after a provider loss
    /// (Paykit spec 6).
    pub divergent: Vec<Divergence>,
    /// The app record's upkeep at this receive failed short of a wallet-wide stop: why. The
    /// record is tried again at the next receive, or at once by `update_own_record`, which
    /// returns its outcome; every other step of the receive ran (Paykit spec 4).
    pub app_record_owed: Option<String>,
}
/// What a wait for the feed ended in (`FeedWait::wait`).
#[derive(Clone, Copy, uniffi::Enum)]
pub enum FeedWake {
    /// The feed shows something past the position waited from: the host calls `App::receive` with
    /// no wait.
    Ready,
    /// The wait passed with nothing to show: the host takes a fresh handle and waits again.
    Quiet,
    /// The app should read now: the switch refused the wait or couldn't be reached, an answer
    /// didn't verify with the key the app pinned (a recovery in place answers a held read so,
    /// Switch wire 7.4.4), the account is frozen, or a failed call left state only a read loads
    /// again.
    ReadNow,
    /// Reads stop where the last one did, for something no wait on the feed shows, and the handle
    /// sent nothing: the last read stopped (`ReceiveReport::stopped`: a key another device made
    /// that this one lacks, a provider change or a move awaited), its owed listing waits for a
    /// scope key (`AppEvent::ListingWaitsForKey`), or the app is removed, gone, departed or
    /// stopped on an alarm.
    ReadLater,
    /// A native case that this binding does not recognize.
    Other,
}

#[uniffi::export]
impl NativeWallet {
    /// Wait on the feed and apply the native loop's retry pause before returning.
    /// The wait holds up no other wallet call. A host that stops waiting ends the wait and
    /// leaves the durable cursor unchanged.
    pub async fn feed_wait(&self, seconds: u32) -> Result<FeedWake, WalletError> {
        let wait = self
            .with_wallet(move |app| Box::pin(async move { Ok(app.app().feed_wait(seconds)) }))
            .await?;
        let pace = self.pace.clone();
        // A host that stops waiting (a cancelled Kotlin coroutine) ends the long poll and the
        // pause, so a restarted feed loop never holds two reads at once. A cancelled Swift task
        // keeps waiting for the answer.
        crate::runtime::run_while_waited(async move {
            let requested = wait.seconds();
            let started = std::time::Instant::now();
            let wake = wait.wait().await;
            let pause = pace
                .lock()
                .await
                .after_wait(wake, requested, started.elapsed());
            if let Some(pause) = pause {
                tokio::time::sleep(pause).await;
            }
            Ok(match wake {
                Woken::Ready => FeedWake::Ready,
                Woken::Quiet => FeedWake::Quiet,
                Woken::ReadNow => FeedWake::ReadNow,
                Woken::ReadLater => FeedWake::ReadLater,
                _ => FeedWake::Other,
            })
        })
        .await
    }
    /// Delay in milliseconds before retrying a failed receive in this wallet's feed loop.
    pub async fn failed_read_pause_ms(&self) -> u64 {
        self.pace.lock().await.after_failed_read().as_millis() as u64
    }
    /// Process one bounded receive. Persist the handled effects before acknowledging their prefix.
    pub async fn receive(&self) -> Result<WalletReceiveReport, WalletError> {
        self.with_wallet(move |app| {
            Box::pin(async move {
                let s = app.receive(None).await?;
                let r = s.report;
                Ok(WalletReceiveReport {
                    generation: r.cursor.generation,
                    sequence: r.cursor.seq,
                    stopped: r.stopped,
                    more: r.more,
                    held_cursor: r.held_cursor.map(Into::into),
                    events: r.events.into_iter().map(|e| event(app, e)).collect(),
                    applied: s.applied,
                    resends: s.resends,
                    resend_refused: s.resend_refused.map(|r| Arc::new(PaymentRefusal(r))),
                    certificate_refused: s.certificate_refused.map(|r| Arc::new(PaymentRefusal(r))),
                    records_unlisted: s.records_unlisted,
                    scope_keys_awaited: s.scope_keys_awaited,
                    key_epochs_awaited: s.key_epochs_awaited,
                    account_gone: s.account_gone,
                    divergent: s.divergent.into_iter().map(Into::into).collect(),
                    app_record_owed: s.app_record_owed,
                })
            })
        })
        .await
    }
    /// Read the verified notice currently holding this installation, independently of
    /// whether the host has already acknowledged its delivery (Switch spec 12.6).
    pub async fn recovery_required(
        &self,
    ) -> Result<Option<crate::WalletRecoveryNotice>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move { Ok(app.app().recovery_required().map(Into::into)) })
        })
        .await
    }

    /// Recover the outstanding Paykit-owned duties after reopening or a failed receive.
    pub async fn unacknowledged_events(&self) -> Result<Vec<WalletEvent>, WalletError> {
        self.with_wallet(move |app| {
            Box::pin(async move {
                Ok(app
                    .app_mut()
                    .unacknowledged_events_for("paykit")
                    .await?
                    .into_iter()
                    .map(|e| event(app, e))
                    .collect())
            })
        })
        .await
    }
    /// Acknowledge the already handled Paykit prefix through the last duty in a successful report.
    pub async fn acknowledge_events(&self, through: Vec<u8>) -> Result<(), WalletError> {
        let id = EventId::from_bytes(&through).map_err(|_| WalletError::invalid("event id"))?;
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.app_mut().ack_events_for("paykit", id).await?) })
        })
        .await
    }
}
