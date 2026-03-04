//! Authenticated Pubky adapter that satisfies [`crate::AuthenticatedTransport`].

use async_trait::async_trait;
use pubky::PubkySession;
use tracing::{debug, error, instrument};

use super::{PAYKIT_PATH_PREFIX, PAYKIT_PRIVATE_PATH_PREFIX, PAYKIT_PRIVATE_PAYMENTS_FILE};
use crate::transport::traits::AuthenticatedTransport;
use crate::{EndpointData, MethodId, PaykitError, Result};

/// Adapter around `pubky::PubkySession` implementing `AuthenticatedTransport`.
#[derive(Clone)]
pub struct PubkyAuthenticatedTransport {
    session: PubkySession,
}

impl PubkyAuthenticatedTransport {
    /// Create a new adapter from an existing session.
    pub fn new(session: PubkySession) -> Self {
        Self { session }
    }

    /// Access the wrapped session for advanced payers/payees.
    pub fn session(&self) -> &PubkySession {
        &self.session
    }
}

impl From<PubkySession> for PubkyAuthenticatedTransport {
    fn from(session: PubkySession) -> Self {
        Self { session }
    }
}

#[async_trait]
impl AuthenticatedTransport for PubkyAuthenticatedTransport {
    #[instrument(skip(self, data), fields(method = %method))]
    async fn upsert_payment_endpoint(&self, method: &MethodId, data: &EndpointData) -> Result<()> {
        let path = format!("{PAYKIT_PATH_PREFIX}{}", method.as_str());
        debug!(path = %path, "writing payment endpoint to storage");
        self.session
            .storage()
            .put(path, data.as_str().to_string())
            .await
            .map_err(|err| {
                error!(error = %err, "failed to put payment endpoint");
                PaykitError::Transport {
                    context: "put endpoint".into(),
                    source: err.into(),
                }
            })?;
        debug!("payment endpoint stored successfully");
        Ok(())
    }

    #[instrument(skip(self), fields(method = %method))]
    async fn remove_payment_endpoint(&self, method: &MethodId) -> Result<()> {
        let path = format!("{PAYKIT_PATH_PREFIX}{}", method.as_str());
        debug!(path = %path, "deleting payment endpoint from storage");
        self.session.storage().delete(path).await.map_err(|err| {
            error!(error = %err, "failed to delete payment endpoint");
            PaykitError::Transport {
                context: "delete endpoint".into(),
                source: err.into(),
            }
        })?;
        debug!("payment endpoint removed successfully");
        Ok(())
    }

    #[instrument(skip(self, data), fields(recipient_id = %recipient_id))]
    async fn put_private_payments(&self, recipient_id: &str, data: &[u8]) -> Result<()> {
        let path =
            format!("{PAYKIT_PRIVATE_PATH_PREFIX}{recipient_id}/{PAYKIT_PRIVATE_PAYMENTS_FILE}");
        debug!(path = %path, len = data.len(), "writing private payments blob to storage");
        self.session
            .storage()
            .put(path, data.to_vec())
            .await
            .map_err(|err| {
                error!(error = %err, "failed to put private payments blob");
                PaykitError::Transport {
                    context: "put private payments".into(),
                    source: err.into(),
                }
            })?;
        debug!("private payments blob stored successfully");
        Ok(())
    }

    #[instrument(skip(self), fields(recipient_id = %recipient_id))]
    async fn remove_private_payments(&self, recipient_id: &str) -> Result<()> {
        let path =
            format!("{PAYKIT_PRIVATE_PATH_PREFIX}{recipient_id}/{PAYKIT_PRIVATE_PAYMENTS_FILE}");
        debug!(path = %path, "deleting private payments blob from storage");
        self.session.storage().delete(path).await.map_err(|err| {
            error!(error = %err, "failed to delete private payments blob");
            PaykitError::Transport {
                context: "delete private payments".into(),
                source: err.into(),
            }
        })?;
        debug!("private payments blob removed successfully");
        Ok(())
    }
}
