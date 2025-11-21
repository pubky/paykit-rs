//! Authenticated Pubky adapter that satisfies [`crate::AuthenticatedTransport`].

use async_trait::async_trait;
use pubky::PubkySession;

use super::PAYKIT_PATH_PREFIX;
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
    async fn upsert_payment_endpoint(&self, method: &MethodId, data: &EndpointData) -> Result<()> {
        let path = format!("{PAYKIT_PATH_PREFIX}{}", method.0);
        self.session
            .storage()
            .put(path, data.0.clone())
            .await
            .map_err(|err| PaykitError::Transport(format!("put endpoint: {err}")))?;
        Ok(())
    }

    async fn remove_payment_endpoint(&self, method: &MethodId) -> Result<()> {
        let path = format!("{PAYKIT_PATH_PREFIX}{}", method.0);
        self.session
            .storage()
            .delete(path)
            .await
            .map_err(|err| PaykitError::Transport(format!("delete endpoint: {err}")))?;
        Ok(())
    }
}
