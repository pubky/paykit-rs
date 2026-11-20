use crate::{fixed, CursorStatus, NativeWallet, SwitchAlarm, WalletError, WalletHost};
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

#[uniffi::export(async_runtime = "tokio")]
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
        if matches!(source_device, BackupDevice::UnaccountedFor) {
            return Err(WalletError::SourceDeviceUnaccountedFor);
        }
        let id = fixed(app_id, "app id")?;
        let backup = PaykitBackup::from_bytes(&bytes)?;
        let (env, owner) = self.session_env(id).await?;
        let app = Paykit::restore(
            env,
            Default::default(),
            &backup,
            self.rail.clone(),
            self.timer.clone(),
        )
        .await?;
        Ok(NativeWallet::opened(app, Some(owner)))
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl NativeWallet {
    /// Inspect the installed state before or after a foreground read, including after reopening.
    pub async fn restore_state(&self) -> Result<WalletRestoreState, WalletError> {
        self.with_wallet(async |wallet| {
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
        .await
    }

    /// The user recognizes every displayed registration as the original device's own use.
    pub async fn confirm_restored_registrations(
        &self,
        registrations: Vec<u64>,
    ) -> Result<(), WalletError> {
        self.with_wallet(async |app| {
            Ok(app
                .confirm_restored_registrations(&registrations, UserConfirmed::new())
                .await?)
        })
        .await
    }

    /// The user cannot account for the displayed registrations. Switch stops this app and
    /// requests revocation. The resulting alarm remains available in the restore state.
    pub async fn refuse_restored_registrations(
        &self,
        registrations: Vec<u64>,
    ) -> Result<(), WalletError> {
        self.with_wallet(async |app| Ok(app.refuse_restored_registrations(&registrations).await?))
            .await
    }
}
