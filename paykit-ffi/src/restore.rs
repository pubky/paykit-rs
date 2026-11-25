use crate::{
    fixed, CursorStatus, NativeWallet, ProviderLossBoundary, SwitchAlarm, WalletError, WalletHost,
};
use paykit_sdk::{Paykit, PaykitBackup, UserConfirmed};
use std::sync::Arc;

/// Whether the user can still account for the source device (Switch spec 12.4).
#[derive(uniffi::Enum)]
pub enum BackupDevice {
    /// The device is still in the user's possession.
    InPossession,
    /// The user knows that the previous device was wiped.
    KnownWiped,
    /// The source device was lost, stolen or its custody is unknown.
    UnaccountedFor,
}

/// The installed client's current restore evidence (Switch spec 12.4).
#[derive(uniffi::Record)]
pub struct WalletRestoreState {
    /// The app remains in reconciliation, including while another registered protocol is reconciling.
    pub reconciling: bool,
    /// The cursor verdict from the verified service read, when available.
    pub cursor: Option<CursorStatus>,
    /// Registrations since the backup whose use the user must account for.
    pub registrations: Vec<u64>,
    /// An alarm that stopped the installation, including a refused registration.
    pub alarm: Option<SwitchAlarm>,
}

/// Original apps, verified reports and explicit removals for one provider loss (Paykit spec 7.3).
#[derive(uniffi::Record)]
pub struct RecoveryMembers {
    /// The loss covered by the identity-signed opening (Paykit spec 7.3).
    pub loss: ProviderLossBoundary,
    /// The 32-byte hash of the opening identity document (Paykit wire 5.10).
    pub opening: Vec<u8>,
    /// The 16-byte App IDs on the opening with no verified removal (Paykit spec 7.3).
    pub originals: Vec<Vec<u8>>,
    /// Verified original reports, including any from apps removed later (Paykit wire 5.10).
    pub reports: Vec<RecoveryReport>,
    /// The 16-byte App IDs explicitly removed since the opening (Paykit spec 7.3).
    pub removed: Vec<Vec<u8>>,
}

impl From<paykit_sdk::RecoveryMembers> for RecoveryMembers {
    fn from(members: paykit_sdk::RecoveryMembers) -> Self {
        Self {
            loss: members.loss.into(),
            opening: members.opening.into(),
            originals: members.originals.into_iter().map(Vec::from).collect(),
            reports: members.reports.into_iter().map(Into::into).collect(),
            removed: members.removed.into_iter().map(Vec::from).collect(),
        }
    }
}

/// An original app's signed commitments and their verified publication (Paykit wire 5.10).
#[derive(uniffi::Record)]
pub struct RecoveryReport {
    /// The 16-byte ID of the original app that signed the report (Paykit wire 5.10).
    pub original: Vec<u8>,
    /// The authorized publisher's 16-byte App ID (Paykit wire 5.10).
    pub publisher: Vec<u8>,
    /// The exact write accepted by the service (Switch wire 6.3).
    pub application: ApplicationRef,
    /// The 32-byte commitment to the original app's inventory cutoff (Paykit wire 5.10).
    pub cutoff: Vec<u8>,
    /// The 32-byte commitment to the inventory at the cutoff (Paykit wire 5.10).
    pub inventory: Vec<u8>,
    /// The 32-byte commitment to the decisions for those items (Paykit wire 5.10).
    pub dispositions: Vec<u8>,
}

impl From<paykit_sdk::RecoveryReport> for RecoveryReport {
    fn from(report: paykit_sdk::RecoveryReport) -> Self {
        Self {
            original: report.original.into(),
            publisher: report.publisher.into(),
            application: ApplicationRef {
                generation: report.application.generation,
                seq: report.application.seq,
                item_hash: report.application.item_hash.into(),
            },
            cutoff: report.cutoff.into(),
            inventory: report.inventory.into(),
            dispositions: report.dispositions.into(),
        }
    }
}

/// The service's reference to an applied record write (Switch wire 6.3).
#[derive(uniffi::Record)]
pub struct ApplicationRef {
    /// The generation in which the write was applied (Switch wire 6.3).
    pub generation: u64,
    /// The sequence of the write within that generation (Switch wire 6.3).
    pub seq: u64,
    /// The 32-byte hash of the full applied item (Switch wire 6.3).
    pub item_hash: Vec<u8>,
}

#[uniffi::export]
impl WalletHost {
    /// Ask the user about the source device first (Switch spec 12.4). A lost or stolen
    /// device requires revoking the old app and enrolling new keys.
    /// Supply the App ID saved alongside the protected backup. The exclusive store
    /// rejects reads and writes to any other app before an installation can change.
    pub async fn restore(
        &self,
        app_id: Vec<u8>,
        bytes: Vec<u8>,
        source_device: BackupDevice,
    ) -> Result<Arc<NativeWallet>, WalletError> {
        let this = self.clone();
        crate::runtime::run(async move {
            if matches!(source_device, BackupDevice::UnaccountedFor) {
                return Err(WalletError::SourceDeviceUnaccountedFor);
            }
            let id = fixed(app_id, "app id")?;
            // Bytes that don't read as a backup are the caller's input, refused before the
            // wallet opens.
            let backup =
                PaykitBackup::from_bytes(&bytes).map_err(|_| WalletError::invalid("backup"))?;
            this.open_wallet(id, async |env| {
                Ok(Paykit::restore(
                    env,
                    this.config.clone(),
                    &backup,
                    this.rail.clone(),
                    this.timer.clone(),
                )
                .await?)
            })
            .await
        })
        .await
    }
}

#[uniffi::export]
impl NativeWallet {
    /// Read the opening, removals and verified reports for the loss this wallet follows,
    /// or return none when there is no loss. The current recovery grant and key are
    /// required. Membership alone settles no payment or credit. An unavailable grant,
    /// key or opening gives `NotNow`. A different loss gives `LossChanged` (Paykit spec 7.3).
    pub async fn recovery_members(&self) -> Result<Option<RecoveryMembers>, WalletError> {
        self.with_wallet(|app| {
            Box::pin(async move { Ok(app.recovery_members().await?.map(Into::into)) })
        })
        .await
    }

    /// Inspect the installed state before or after a foreground read, including after reopening.
    pub async fn restore_state(&self) -> Result<WalletRestoreState, WalletError> {
        self.with_wallet(move |wallet| {
            Box::pin(async move {
                let app = wallet.app_mut();
                // Local inspection reloads a failed commit without handing duties to the host.
                app.pending_duty_events().await?;
                Ok(WalletRestoreState {
                    reconciling: app.reconcile_mode(),
                    cursor: app.restore_held_cursor().map(Into::into),
                    registrations: app.restore_question(),
                    alarm: app.alarm().cloned().map(Into::into),
                })
            })
        })
        .await
    }

    /// The user recognizes every displayed registration as the original device's own use.
    pub async fn confirm_restored_registrations(
        &self,
        registrations: Vec<u64>,
    ) -> Result<(), WalletError> {
        self.with_wallet(move |app| {
            Box::pin(async move {
                Ok(app
                    .confirm_restored_registrations(&registrations, UserConfirmed::new())
                    .await?)
            })
        })
        .await
    }

    /// The user cannot account for the displayed registrations. Switch stops this app and
    /// requests revocation. The resulting alarm remains available in the restore state.
    pub async fn refuse_restored_registrations(
        &self,
        registrations: Vec<u64>,
    ) -> Result<(), WalletError> {
        self.with_wallet(move |app| {
            Box::pin(async move { Ok(app.refuse_restored_registrations(&registrations).await?) })
        })
        .await
    }
}
