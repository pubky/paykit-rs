use crate::{HostRail, HostStore, PaymentMethod, WalletError, WalletStore};
use paykit_sdk::{AppEnv, PaymentEndpointIdentifier, Rail, Timer};
use pubky_switch_client::app::{HomeserverReader, HomeserverWriter};
use pubky_switch_client::clock::SystemClock;
use pubky_switch_client::store::StoreHandle;
use pubky_switch_client::transport::{BoxFuture, HttpConfig, HttpTransport, TransportError};
use std::{sync::Arc, time::Duration};

/// Failure reading or writing a Pubky Homeserver path.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum HomeserverError {
    /// The Homeserver did not answer before the timeout.
    #[error("the homeserver timed out")]
    Timeout,
    /// The Homeserver could not be reached.
    #[error("the homeserver could not be reached")]
    Unavailable,
    /// The session lacked authority or the Homeserver refused the operation.
    #[error("the homeserver refused the operation")]
    Refused,
    /// The response exceeded the host's byte limit.
    #[error("the homeserver response exceeds the size limit")]
    TooLarge {
        /// Maximum permitted bytes.
        limit: u64,
    },
}
impl From<HomeserverError> for TransportError {
    fn from(e: HomeserverError) -> Self {
        match e {
            HomeserverError::Timeout => Self::Timeout,
            HomeserverError::Unavailable => Self::Connect("homeserver unavailable".into()),
            HomeserverError::Refused => Self::Refused("homeserver refused access".into()),
            HomeserverError::TooLarge { limit } => Self::TooLarge(limit),
        }
    }
}
/// Use the authenticated session for the given identity on writes. A missing public
/// file is a successful null read. A successful write or deletion is already durable.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait WalletHomeserver: Send + Sync {
    /// Read the named public path. Return null only for an absent file.
    async fn get(
        &self,
        identity: Vec<u8>,
        path: String,
    ) -> Result<Option<Vec<u8>>, HomeserverError>;
    /// Write the file durably through the named identity's authenticated session.
    async fn put(
        &self,
        identity: Vec<u8>,
        path: String,
        bytes: Vec<u8>,
    ) -> Result<(), HomeserverError>;
    /// Remove the file through the named identity's authenticated session.
    async fn delete(&self, identity: Vec<u8>, path: String) -> Result<(), HomeserverError>;
}
struct Home(Arc<dyn WalletHomeserver>);
impl HomeserverReader for Home {
    fn get(
        &self,
        identity: [u8; 32],
        path: String,
    ) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        Box::pin(async move { Ok(self.0.get(identity.to_vec(), path).await?) })
    }
}
impl HomeserverWriter for Home {
    fn put(
        &self,
        identity: [u8; 32],
        path: String,
        bytes: Vec<u8>,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move { Ok(self.0.put(identity.to_vec(), path, bytes).await?) })
    }
    fn delete(
        &self,
        identity: [u8; 32],
        path: String,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move { Ok(self.0.delete(identity.to_vec(), path).await?) })
    }
}
pub(crate) struct RuntimeTimer;
impl Timer for RuntimeTimer {
    fn sleep(&self, ms: u64) -> BoxFuture<'static, ()> {
        Box::pin(async move { tokio::time::sleep(Duration::from_millis(ms)).await })
    }
}
/// A Paykit application's protected store, Pubky session and payment method.
#[derive(uniffi::Object)]
pub struct WalletHost {
    pub(crate) env: AppEnv,
    pub(crate) store: Arc<dyn WalletStore>,
    pub(crate) rail: Arc<dyn Rail>,
    pub(crate) timer: Arc<dyn Timer>,
}
#[uniffi::export]
impl WalletHost {
    /// Use this protected store, Pubky session and payment method for wallet operations.
    #[uniffi::constructor]
    pub fn new(
        store: Arc<dyn WalletStore>,
        homeserver: Arc<dyn WalletHomeserver>,
        method: Arc<dyn PaymentMethod>,
        endpoint_identifiers: Vec<String>,
    ) -> Result<Arc<Self>, WalletError> {
        let identifiers = endpoint_identifiers
            .iter()
            .map(|id| PaymentEndpointIdentifier::new(id))
            .collect::<Result<_, _>>()
            .map_err(|_| WalletError::invalid("payment endpoint identifier"))?;
        let home = Arc::new(Home(homeserver));
        let transport = HttpTransport::new(HttpConfig::default())
            .map_err(|_| WalletError::invalid("HTTPS transport configuration"))?;
        let env = AppEnv {
            store: StoreHandle::new(Arc::new(HostStore(store.clone()))),
            transport: Arc::new(transport),
            clock: Arc::new(SystemClock),
            homeserver: home.clone(),
            homeserver_writer: Some(home),
        };
        Ok(Arc::new(Self {
            env,
            store,
            rail: Arc::new(HostRail {
                method,
                identifiers,
            }),
            timer: Arc::new(RuntimeTimer),
        }))
    }
}
impl From<uniffi::UnexpectedUniFFICallbackError> for HomeserverError {
    fn from(_: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Unavailable
    }
}
