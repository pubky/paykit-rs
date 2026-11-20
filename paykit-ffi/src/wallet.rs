use crate::{fixed, WalletError, WalletHost};
use paykit_sdk::{AppEnv, Paykit, PaykitBackup, PaykitError};
use pubky_switch_client::store::StoreHandle;
use std::sync::Arc;
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard};

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

/// One open, exclusively owned Paykit wallet.
#[derive(uniffi::Object)]
pub struct NativeWallet {
    state: Mutex<Option<Paykit>>,
    ownership: Option<Arc<crate::LeaseOwner>>,
}
impl NativeWallet {
    /// Take exclusive ownership of an already opened SDK wallet. Native hosts with other
    /// Switch protocols construct it with the SDK's registration APIs or with
    /// [`Paykit::attach`] using the identical registered Paykit protocol instance.
    /// The host's store must continue to exclude other writers through the handle's lifetime.
    pub fn from_wallet(app: Paykit) -> Arc<Self> {
        Self::opened(app, None)
    }
    pub(crate) fn opened(app: Paykit, ownership: Option<Arc<crate::LeaseOwner>>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(Some(app)),
            ownership,
        })
    }
    /// Take the live state out across a mutation. A dropped future closes this view,
    /// so its replacement always rereads the durable journal and native cursor.
    pub(crate) async fn with_wallet<T>(
        &self,
        call: impl AsyncFnOnce(&mut Paykit) -> Result<T, WalletError>,
    ) -> Result<T, WalletError> {
        let mut slot = self.state.lock().await;
        let mut live = slot.take().ok_or(WalletError::Closed)?;
        let result = call(&mut live).await;
        *slot = Some(live);
        result
    }
    pub(crate) async fn lock(&self) -> Result<MappedMutexGuard<'_, Paykit>, WalletError> {
        MutexGuard::try_map(self.state.lock().await, |s| s.as_mut())
            .map_err(|_| WalletError::Closed)
    }
}
#[uniffi::export(async_runtime = "tokio")]
impl NativeWallet {
    /// Finish the current operation and release writer ownership so another handle can load.
    pub async fn shutdown(&self) -> Result<(), WalletError> {
        self.state.lock().await.take();
        if let Some(owner) = &self.ownership {
            owner.finish().await?;
        }
        Ok(())
    }
    /// Read this installation's App ID.
    pub async fn app_id(&self) -> Result<Vec<u8>, WalletError> {
        Ok(self.lock().await?.app_id().into())
    }
    /// Read the identity key that approved this wallet.
    pub async fn identity(&self) -> Result<Vec<u8>, WalletError> {
        Ok(self.lock().await?.identity().into())
    }
    /// Produce the SDK backup for explicit protected storage by the host.
    pub async fn backup(&self) -> Result<Vec<u8>, WalletError> {
        self.with_wallet(async |app| Ok(app.backup().await?.to_bytes()?))
            .await
    }
}
impl WalletHost {
    pub(crate) async fn session_env(
        &self,
        id: [u8; 16],
    ) -> Result<(AppEnv, Arc<crate::LeaseOwner>), WalletError> {
        let mut env = self.env.clone();
        let session = crate::WalletStoreSession::open(self.store.clone(), id).await?;
        let owner = session.ownership.clone();
        env.store = StoreHandle::new(session);
        Ok((env, owner))
    }
    pub(crate) async fn load_app(&self, env: AppEnv, id: [u8; 16]) -> Result<Paykit, PaykitError> {
        Paykit::load(
            env,
            Default::default(),
            id,
            self.rail.clone(),
            self.timer.clone(),
        )
        .await
    }
}
#[uniffi::export(async_runtime = "tokio")]
impl WalletHost {
    /// Open the existing wallet under its original App ID and exclusive storage lease.
    pub async fn load(&self, app_id: Vec<u8>) -> Result<Arc<NativeWallet>, WalletError> {
        let id = fixed(app_id, "app id")?;
        let (env, owner) = self.session_env(id).await?;
        Ok(NativeWallet::opened(
            self.load_app(env, id).await?,
            Some(owner),
        ))
    }
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
