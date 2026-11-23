use crate::{
    EndpointSupport, HostRail, MethodDeclaration, PaymentMethod, WalletError, WalletStore,
};
use paykit_sdk::{PaymentEndpointIdentifier, Timer};
use pubky_switch_client::app::{HomeserverReader, HomeserverWriter};
use pubky_switch_client::clock::{Clock, SystemClock};

use crate::runtime::HOST_CALL_LIMIT;
use pubky_switch_client::transport::{
    BoxFuture, HttpConfig, HttpTransport, Transport, TransportError,
};
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

/// Wait for a host Homeserver call at most [`HOST_CALL_LIMIT`]. Past it the call counts as
/// timed out, which leaves a write's outcome unknown, as for any transport timeout.
async fn bounded<T>(
    call: impl std::future::Future<Output = Result<T, HomeserverError>>,
) -> Result<T, TransportError> {
    Ok(tokio::time::timeout(HOST_CALL_LIMIT, call)
        .await
        .map_err(|_| TransportError::Timeout)??)
}
impl HomeserverReader for Home {
    fn get(
        &self,
        identity: [u8; 32],
        path: String,
    ) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        Box::pin(bounded(self.0.get(identity.to_vec(), path)))
    }
}
impl HomeserverWriter for Home {
    fn put(
        &self,
        identity: [u8; 32],
        path: String,
        bytes: Vec<u8>,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(bounded(self.0.put(identity.to_vec(), path, bytes)))
    }
    fn delete(
        &self,
        identity: [u8; 32],
        path: String,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(bounded(self.0.delete(identity.to_vec(), path)))
    }
}
pub(crate) struct RuntimeTimer;
impl Timer for RuntimeTimer {
    fn sleep(&self, ms: u64) -> BoxFuture<'static, ()> {
        Box::pin(async move { tokio::time::sleep(Duration::from_millis(ms)).await })
    }
}
/// A Paykit application's protected store, Pubky session and payment method.
#[derive(Clone, uniffi::Object)]
pub struct WalletHost {
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) transport: Arc<dyn Transport>,
    pub(crate) home: Arc<dyn HomeserverReader>,
    pub(crate) writer: Arc<dyn HomeserverWriter>,
    pub(crate) store: Arc<dyn WalletStore>,
    pub(crate) rail: Arc<HostRail>,
    pub(crate) timer: Arc<dyn Timer>,
}
#[uniffi::export]
impl WalletHost {
    /// Use this protected store, Pubky session and payment method for wallet operations. The
    /// method's declaration and the endpoints it supports are given here, once: a method whose
    /// guarantees change is given to a new `WalletHost`.
    #[uniffi::constructor]
    pub fn new(
        store: Arc<dyn WalletStore>,
        homeserver: Arc<dyn WalletHomeserver>,
        method: Arc<dyn PaymentMethod>,
        declaration: MethodDeclaration,
        endpoints: Vec<EndpointSupport>,
    ) -> Result<Arc<Self>, WalletError> {
        let endpoints = supported_endpoints(endpoints)?;
        let home = Arc::new(Home(homeserver));
        let transport = HttpTransport::new(HttpConfig::default())
            .map_err(|_| WalletError::invalid("HTTPS transport configuration"))?;
        Ok(Arc::new(Self {
            clock: Arc::new(SystemClock),
            transport: Arc::new(transport),
            home: home.clone(),
            writer: home,
            store,
            rail: Arc::new(HostRail::new(method, declaration, endpoints)),
            timer: Arc::new(RuntimeTimer),
        }))
    }
}
impl From<uniffi::UnexpectedUniFFICallbackError> for HomeserverError {
    fn from(_: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Unavailable
    }
}

/// Validate the endpoints a host says its method pays, with their fractional digits.
pub(crate) fn supported_endpoints(
    endpoints: Vec<EndpointSupport>,
) -> Result<Vec<(PaymentEndpointIdentifier, Option<u32>)>, WalletError> {
    let mut supported: Vec<(PaymentEndpointIdentifier, Option<u32>)> = Vec::new();
    for e in endpoints {
        let id = PaymentEndpointIdentifier::new(&e.identifier)
            .map_err(|_| WalletError::invalid("payment endpoint identifier"))?;
        if supported.iter().any(|(known, _)| *known == id) {
            return Err(WalletError::invalid("payment endpoint identifier"));
        }
        // Amounts carry at most 18 fractional digits (Paykit wire 1.2).
        if e.fraction_digits
            .is_some_and(|d| d as usize > paykit_lib::amount::MAX_FRACTION_DIGITS)
        {
            return Err(WalletError::invalid("fraction digits"));
        }
        supported.push((id, e.fraction_digits));
    }
    Ok(supported)
}
